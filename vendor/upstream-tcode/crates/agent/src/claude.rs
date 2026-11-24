//! Claude Code provider: spawns the `claude` CLI as a persistent child process
//! per session and speaks the bidirectional stream-json protocol.
//!
//! The CLI is launched as:
//!
//! ```text
//! claude --print --input-format stream-json --output-format stream-json \
//!        --include-partial-messages --verbose --permission-prompt-tool stdio \
//!        [--model <model>] [--resume <session_id>]
//! ```
//!
//! `--permission-prompt-tool stdio` makes the CLI ask for tool-use permission
//! over the control protocol (`control_request` with subtype `can_use_tool`),
//! which we surface as [`AgentEvent::ApprovalRequested`] and answer with a
//! `control_response`. This flag set (and the control shapes) is lifted from the
//! `@anthropic-ai/claude-agent-sdk` `Query` implementation, which spawns the same
//! CLI. We intentionally do NOT send an `initialize` control_request: the CLI
//! streams `can_use_tool` prompts without it (verified against v2.1.206), so the
//! handshake is unnecessary for our reduced feature set.
//!
//! Everything is normalized into the canonical [`AgentEvent`] stream. An actor
//! task owns the child: it reads stdout lines, receives [`SessionCommand`]s, and
//! writes stream-json lines to stdin. Multiple turns run over one process.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use crate::{LaterHistory, RewindFailure, RewindFailureReason};
use crate::{RewindFile, RewindFiles, RewindIncomplete};
use smol::io::{AsyncWrite, BufReader};
use smol::prelude::*;
use smol::process::Stdio;

pub use crate::claude_context::{CONTEXT_WINDOW_SUFFIXES, strip_context_window_suffix};
use crate::claude_goal::GoalTranscript;
use crate::claude_manifest::{CatalogModel, ClaudeCatalog};
use crate::{
    AgentError, AgentEvent, ApprovalDecision, ApprovalKind, ApprovalOption, ApprovalOptionKind,
    ApprovalRequest, Attachment, CatalogRefresh, ClassifierCategory, DeltaKind, FileChange,
    FileChangeKind, GoalStatus, ItemContent, ItemStatus, LaunchEnv, ModelSpec, OptionDescriptor,
    OptionSelection, PlanStep, PlanStepStatus, ProviderCommand, ProviderCommandKind, ProviderKind,
    ResumeCursor, RewindMode, SessionCommand, SessionHandle, SessionOptions, ThreadItem,
    TokenUsage, TurnStatus, UserInputDelivery, UserInputOption, UserInputQuestion, selection_bool,
    selection_str,
};

/// First Claude Code build whose headless control protocol is verified to
/// expose `rewind_conversation` alongside the SDK's `rewind_files` request.
const NATIVE_REWIND_MIN_VERSION: (u32, u32, u32) = (2, 1, 214);

/// Resolve the last permission-mode value on the effective argv. Provider
/// launch arguments intentionally follow internal arguments and may use either
/// CLI spelling, so the mode tracker must honor both.
fn effective_permission_mode(internal: &str, extra_args: &[String]) -> String {
    let mut effective = internal.to_owned();
    let mut args = extra_args.iter();
    while let Some(arg) = args.next() {
        if arg == "--permission-mode" {
            if let Some(value) = args.next() {
                effective.clone_from(value);
            }
        } else if let Some(value) = arg.strip_prefix("--permission-mode=") {
            effective = value.to_owned();
        }
    }
    effective
}

/// Start (or resume) a Claude Code session.
pub async fn start(opts: SessionOptions) -> Result<SessionHandle, AgentError> {
    let native_rewind = claude_version(opts.binary_path.as_deref(), &opts.launch_env)
        .await
        .is_some_and(|version| version >= NATIVE_REWIND_MIN_VERSION);
    // Absolute path: a bare name would be resolved against the session cwd we
    // set below, which breaks PATH lookup (see `resolve_binary`).
    let binary = crate::resolve_binary(opts.binary_path.as_deref(), "claude")?;
    let binary = binary.to_string_lossy().into_owned();

    // Resolve model-scoped launch options from the persisted selections
    // (effort/context/fast/thinking are launch-time only; mid-session changes
    // ride the resume-restart machinery).
    let launch = ClaudeLaunchOptions::resolve(opts.model.as_deref(), &opts.option_selections);
    let launch_permission_mode = opts
        .option_selections
        .iter()
        .find(|selection| selection.id == "permissionMode")
        .and_then(|selection| selection.value.as_str())
        .unwrap_or("default");
    // Launch arguments are deliberately appended last, so the tracker must
    // follow their effective value rather than the internal flag they replace.
    let applied_permission_mode =
        effective_permission_mode(launch_permission_mode, &opts.extra_args);

    let mut cmd = crate::process::async_command(&binary);
    cmd.arg("--print")
        .arg("--input-format")
        .arg("stream-json")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--include-partial-messages")
        .arg("--verbose")
        .arg("--permission-prompt-tool")
        .arg("stdio")
        .arg("--permission-mode")
        .arg(launch_permission_mode)
        .arg("--allow-dangerously-skip-permissions");

    if native_rewind {
        // User-message UUIDs are Claude's native checkpoint ids. SDK file
        // checkpointing is opt-in for print/stream-json hosts even though the
        // interactive CLI enables it automatically.
        cmd.arg("--replay-user-messages")
            .env("CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING", "true");
    }

    if let Some(model) = &launch.model_id {
        cmd.arg("--model").arg(model);
    }
    if let Some(effort) = &launch.effort {
        cmd.arg("--effort").arg(effort);
    }
    let mut settings:Value=launch.settings_json.as_deref().and_then(|value|serde_json::from_str(value).ok()).unwrap_or_else(||json!({}));
    if let Some(hooks)=opts.launch_env.env.iter().find(|(key,_)|key=="MUTEKI_SRC_HOOK_SETTINGS").and_then(|(_,value)|serde_json::from_str::<Value>(value).ok()){
        settings["hooks"]=hooks["hooks"].clone();
    }
    if settings.as_object().is_some_and(|value|!value.is_empty()) {cmd.arg("--settings").arg(settings.to_string());}
    for arg in resume_args(&opts.resume, opts.fork) {
        cmd.arg(arg);
    }
    // Register tcode's enabled HTTP MCP servers. Tokens ride in Authorization
    // headers inside the merged `--mcp-config` JSON.
    for arg in mcp_args(&opts.mcp_servers) {
        cmd.arg(arg);
    }
    // Passed on every launch: the CLI records the rendered system prompt with the conversation
    // and renders it again from the current launch after a compaction.
    if let Some(instructions) = &opts.instructions {
        cmd.arg("--append-system-prompt").arg(instructions);
    }
    // Settings → Providers "Launch arguments", appended last so the user can
    // override anything we set above.
    for arg in &opts.extra_args {
        cmd.arg(arg);
    }
    log::debug!(
        "claude spawn args: model={:?} effort={:?} settings={:?} permission-mode={} effective-permission-mode={}",
        launch.model_id,
        launch.effort,
        launch.settings_json,
        launch_permission_mode,
        applied_permission_mode,
    );

    cmd.current_dir(&opts.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // We are frequently spawned from inside Claude Code itself; strip the
        // markers that tell the CLI it is nested so the child behaves like a
        // top-level invocation.
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT");
    // Per-provider environment (Settings → Providers): custom variables and the
    // `HOME` override that relocates `.claude.json` / `.claude`.
    for (key, value) in opts.launch_env.pairs(ProviderKind::ClaudeCode) {
        cmd.env(key, value);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| AgentError::Spawn(format!("spawning `{binary}`: {e}")))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| AgentError::Spawn("child stdin missing".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AgentError::Spawn("child stdout missing".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AgentError::Spawn("child stderr missing".into()))?;

    let (cmd_tx, cmd_rx) = smol::channel::unbounded::<SessionCommand>();
    let (event_tx, event_rx) = smol::channel::unbounded::<AgentEvent>();
    let _=event_tx.try_send(AgentEvent::ProcessStarted{pid:child.id()});

    // Reader task: forward each stdout line (an item = one JSON message) into an
    // internal channel; closing the channel signals stdout EOF.
    let (line_tx, line_rx) = smol::channel::unbounded::<String>();
    smol::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines.next().await {
            match line {
                Ok(line) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    if line_tx.send(line).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    log::warn!("claude: stdout read error: {e}");
                    break;
                }
            }
        }
        drop(line_tx);
    })
    .detach();

    // Stderr drain: never protocol, but the tail is kept so an unexpected exit
    // can be reported in the CLI's own words (crash stacks land here).
    let stderr_tail = crate::process::StderrTail::default();
    let stderr_task = smol::spawn({
        let tail = stderr_tail.clone();
        async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Some(Ok(line)) = lines.next().await {
                if line.trim().is_empty() {
                    continue;
                }
                log::warn!("claude[stderr]: {line}");
                tail.push(line);
            }
        }
    });

    let session_config = SessionConfig {
        claude_dir: opts
            .launch_env
            .home
            .clone()
            .or_else(|| {
                opts.launch_env
                    .env
                    .iter()
                    .rev()
                    .find(|(key, _)| key == "HOME")
                    .map(|(_, value)| PathBuf::from(value))
            })
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .map(|home| home.join(".claude")),
        resumed: opts.resume.is_some(),
    };
    smol::spawn(actor_loop(
        child,
        stdin,
        cmd_rx,
        line_rx,
        event_tx,
        session_config,
        applied_permission_mode,
        native_rewind,
        opts.model.clone(),
        stderr_tail,
        stderr_task,
    ))
    .detach();

    Ok(SessionHandle {
        provider: ProviderKind::ClaudeCode,
        commands: cmd_tx,
        events: event_rx,
    })
}

/// Ask a short-lived Claude Code process for its subscription usage.
pub async fn read_usage(
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
) -> Result<Value, AgentError> {
    let binary = crate::resolve_binary(binary_path.as_deref(), "claude")?;
    let binary_display = binary.to_string_lossy().into_owned();
    let mut cmd = crate::process::async_command(&binary);
    cmd.arg("--print")
        .arg("--input-format")
        .arg("stream-json")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--verbose")
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT");
    for (key, value) in launch_env.pairs(ProviderKind::ClaudeCode) {
        cmd.env(key, value);
    }

    let mut child = cmd
        .spawn()
        .map_err(|err| AgentError::Spawn(format!("spawning `{binary_display}`: {err}")))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| AgentError::Spawn("child stdin missing".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AgentError::Spawn("child stdout missing".into()))?;

    write_line(
        &mut stdin,
        &json!({
            "type": "control_request",
            "request_id": "usage-1",
            "request": { "subtype": "get_usage" }
        }),
    )
    .await?;

    let read_response = async {
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines.next().await {
            let line = line.map_err(AgentError::Io)?;
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message.get("type").and_then(Value::as_str) != Some("control_response") {
                continue;
            }
            let Some(response) = message.get("response") else {
                continue;
            };
            if response.get("request_id").and_then(Value::as_str) != Some("usage-1") {
                continue;
            }
            if response.get("subtype").and_then(Value::as_str) == Some("error") {
                let error = response
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| response.get("error").unwrap_or(response).to_string());
                return Err(AgentError::Provider(error));
            }
            return response.get("response").cloned().ok_or_else(|| {
                AgentError::Protocol("Claude usage response omitted response".into())
            });
        }
        Err(AgentError::Protocol(
            "Claude stdout closed before usage response".into(),
        ))
    };

    let result = smol::future::or(async { Some(read_response.await) }, async {
        smol::Timer::after(std::time::Duration::from_secs(20)).await;
        None
    })
    .await
    .unwrap_or_else(|| {
        Err(AgentError::Protocol(
            "Claude usage request timed out".into(),
        ))
    });

    let _ = stdin.close().await;
    let _ = child.kill();
    let _ = child.status().await;
    result
}

fn mcp_args(registrations: &[crate::McpRegistration]) -> Vec<String> {
    if registrations.is_empty() {
        Vec::new()
    } else {
        vec![
            "--mcp-config".into(),
            crate::claude_mcp_config_json(registrations),
        ]
    }
}

/// Model-scoped launch flags resolved from the session's option selections.
struct ClaudeLaunchOptions {
    /// Model id with the manifest's context-window suffix (e.g. `[1m]`) only
    /// when the selected window exceeds what the bare slug already opens.
    model_id: Option<String>,
    /// `--effort` value after the manifest's `effortMap` (`None` when the
    /// selection maps to no flag).
    effort: Option<String>,
    /// `--settings` JSON string (fastMode / ultracode / alwaysThinkingEnabled).
    settings_json: Option<String>,
}

fn launch_settings_json(
    thinking: Option<bool>,
    fast_mode: bool,
    ultracode: bool,
    auto_compact_window: Option<u64>,
) -> Option<String> {
    let mut settings = serde_json::Map::new();
    if let Some(thinking) = thinking {
        settings.insert("alwaysThinkingEnabled".into(), json!(thinking));
    }
    if fast_mode {
        settings.insert("fastMode".into(), json!(true));
    }
    if ultracode {
        settings.insert("ultracode".into(), json!(true));
    }
    if let Some(window) = auto_compact_window {
        settings.insert("autoCompactWindow".into(), json!(window));
    }
    (!settings.is_empty())
        .then(|| serde_json::to_string(&Value::Object(settings)).unwrap_or_default())
}

impl ClaudeLaunchOptions {
    fn resolve(model: Option<&str>, selections: &[OptionSelection]) -> Self {
        Self::resolve_with(&crate::claude_manifest::current(), model, selections)
    }

    fn resolve_with(
        catalog: &ClaudeCatalog,
        model: Option<&str>,
        selections: &[OptionSelection],
    ) -> Self {
        let entry = model.and_then(|model| catalog.model(model));
        let spec = entry.map(|entry| &entry.spec);
        let raw_effort = selection_str(selections, "reasoningEffort");
        let resolved_effort = resolve_claude_effort(spec, raw_effort.as_deref());
        let ultracode = resolved_effort.as_deref() == Some("ultracode");
        let effort = match (entry, resolved_effort.as_deref()) {
            (Some(entry), Some(effort)) => entry.cli_effort(effort),
            _ => None,
        };

        let window = catalog.resolved_context_window(model.unwrap_or_default(), selections);
        // Aliases (`opus`, `claude-opus-5.0`) launch as their canonical slug.
        let model_id = model.map(|model| {
            let base = spec.map_or_else(
                || model.split('[').next().unwrap_or(model),
                |spec| spec.id.as_str(),
            );
            let suffix = entry.map_or("", |entry| entry.context_window_suffix(window));
            format!("{base}{suffix}")
        });
        // Anything below the model's largest window is enforced through
        // `autoCompactWindow`: some bare slugs (e.g. Opus 5.5) already run at
        // 1M, so a smaller selection must be told to compact early.
        let largest = entry
            .and_then(CatalogModel::largest_context_window)
            .unwrap_or(window);
        let auto_compact_window = (window < largest).then_some(window);

        // `--settings` object: only supported/true keys are emitted.
        let fast_supported = spec.is_some_and(|spec| has_boolean_option(spec, "fastMode"));
        let thinking_supported = spec.is_some_and(|spec| has_boolean_option(spec, "thinking"));
        let fast_mode = fast_supported && selection_bool(selections, "fastMode") == Some(true);
        let thinking = if thinking_supported {
            selection_bool(selections, "thinking")
        } else {
            None
        };

        let settings_json =
            launch_settings_json(thinking, fast_mode, ultracode, auto_compact_window);

        ClaudeLaunchOptions {
            model_id,
            effort,
            settings_json,
        }
    }
}

/// Per-session config threaded into the actor loop / mapper.
struct SessionConfig {
    claude_dir: Option<PathBuf>,
    /// The transcript already holds history this process did not write.
    resumed: bool,
}

/// The CLI appends transcript records shortly after the stdout lines they
/// accompany, so goal records are polled for a while after each line.
const GOAL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(400);
const GOAL_POLL_WINDOW: std::time::Duration = std::time::Duration::from_secs(3);

fn resume_args(resume: &Option<ResumeCursor>, fork: bool) -> Vec<String> {
    let Some(session_id) = resume
        .as_ref()
        .and_then(|cursor| cursor.str_field(&["session_id"]))
        .map(str::to_owned)
    else {
        return Vec::new();
    };
    let mut args = vec!["--resume".into(), session_id];
    if fork {
        args.push("--fork-session".into());
    }
    args
}

#[allow(clippy::too_many_arguments)]
async fn actor_loop(
    mut child: smol::process::Child,
    mut stdin: smol::process::ChildStdin,
    cmd_rx: smol::channel::Receiver<SessionCommand>,
    line_rx: smol::channel::Receiver<String>,
    event_tx: smol::channel::Sender<AgentEvent>,
    config: SessionConfig,
    applied_permission_mode: String,
    native_rewind: bool,
    expected_model: Option<String>,
    stderr_tail: crate::process::StderrTail,
    stderr_task: smol::Task<()>,
) {
    let base_permission_mode = applied_permission_mode.clone();
    let mut mapper = Mapper::new_configured(applied_permission_mode, native_rewind, expected_model);
    let mut pending_mode_turn: Option<PendingModeTurn> = None;
    let mut pending_option_modes: HashMap<String, std::time::Instant> = HashMap::new();
    let mut deferred_turns = VecDeque::new();
    let claude_dir = config.claude_dir.clone();
    let mut tailers = HashMap::new();
    let (tail_tx, tail_rx) = smol::channel::unbounded::<SubagentTailNotice>();
    let mut goal: Option<GoalTranscript> = None;
    // (next read, last read) while goal records may still be arriving.
    let mut goal_poll: Option<(std::time::Instant, std::time::Instant)> = None;

    // Set when the child died on its own (stdout EOF): only then do its exit
    // status and stderr tail belong in the close reason.
    let mut provider_exited = false;
    let closed_reason: Option<String> = loop {
        // Race a UI command against the next stdout line. `or` biases toward the
        // command channel, which is fine: both channels make independent progress.
        let next_goal_read = goal_poll.map(|(next, _)| next);
        let next_mode_timeout = pending_mode_turn.as_ref().map(|pending| pending.deadline)
            .into_iter().chain(pending_option_modes.values().copied()).min();
        let selection = async { smol::future::or(
            async { Sel::Cmd(cmd_rx.recv().await.ok()) },
            smol::future::or(
                async { Sel::Line(line_rx.recv().await.ok()) },
                smol::future::or(
                    async { Sel::Tail(tail_rx.recv().await.ok()) },
                    smol::future::or(async {
                        match next_goal_read {
                            Some(at) => { smol::Timer::at(at).await; Sel::GoalRead }
                            None => std::future::pending().await,
                        }
                    }, async {
                        match next_mode_timeout {
                            Some(at) => { smol::Timer::at(at).await; Sel::ModeTimeout }
                            None => std::future::pending().await,
                        }
                    }),
                ),
            ),
        ).await };
        let sel = if pending_mode_turn.is_none() && pending_option_modes.is_empty()
            && mapper.current_turn_id.is_none() {
            match deferred_turns.pop_front() {
                Some(command) => Sel::Cmd(Some(command)),
                None => selection.await,
            }
        } else {
            selection.await
        };

        match sel {
            Sel::GoalRead => {
                if let Some(mut reader) = goal.take() {
                    let (reader, changed) = crate::process::unblock(move || {
                        let changed = reader.read_appended();
                        (reader, changed)
                    })
                    .await;
                    goal = Some(reader);
                    if let Some(goal) = changed
                        && event_tx
                            .send(AgentEvent::GoalUpdated { goal })
                            .await
                            .is_err()
                    {
                        let _ = child.kill();
                        return;
                    }
                }
                let now = std::time::Instant::now();
                goal_poll = goal_poll
                    .filter(|(_, until)| now < *until)
                    .map(|(_, until)| (now + GOAL_POLL_INTERVAL, until));
            }
            Sel::ModeTimeout => {
                let now = std::time::Instant::now();
                if pending_option_modes.values().any(|deadline| *deadline <= now) {
                    if pending_mode_turn.take().is_some() {
                        fail_unsent_turn(&mut mapper, &event_tx, "permission_mode_switch",
                            "Claude did not confirm a permission-mode option within 60 seconds".into()).await;
                    }
                    for command in deferred_turns.drain(..) {
                        if matches!(command, SessionCommand::SendTurn { .. }) {
                            fail_unsent_turn(&mut mapper, &event_tx, "permission_mode_switch",
                                "Claude did not confirm a permission-mode option within 60 seconds".into()).await;
                        }
                    }
                    break Some("permission-mode option request timed out".into());
                }
                if let Some(pending) = pending_mode_turn.take() {
                    mapper.pending_permission_modes.remove(&pending.request_id);
                    fail_unsent_turn(&mut mapper, &event_tx, "permission_mode_switch",
                        format!("Claude did not confirm permission mode {} within 60 seconds", pending.mode)).await;
                }
            }
            Sel::Tail(notice) => {
                // The sender side is owned by this loop, so it can't close.
                let Some(notice) = notice else { continue };
                if notice.stopped {
                    tailers.remove(&notice.parent_id);
                }
                for ev in mapper.on_tail_notice(notice) {
                    if event_tx.send(ev).await.is_err() {
                        let _ = child.kill();
                        return;
                    }
                }
            }
            Sel::Cmd(Some(command)) => {
                if matches!(&command, SessionCommand::SendTurn { .. })
                    && (pending_mode_turn.is_some() || !pending_option_modes.is_empty()
                        || mapper.current_turn_id.is_some())
                {
                    deferred_turns.push_back(command);
                    continue;
                }
                if let SessionCommand::SendTurn { options, .. } = &command {
                    let interaction = options.as_ref().and_then(|options| options.interaction_mode.as_deref()).unwrap_or("default");
                    let desired = match interaction {
                        "plan" => "plan",
                        "default" => base_permission_mode.as_str(),
                        other => {
                            fail_unsent_turn(&mut mapper, &event_tx, "interaction_mode_unsupported",
                                format!("Claude does not support interaction mode {other}")).await;
                            continue;
                        }
                    };
                    if desired != mapper.applied_permission_mode {
                        let request = mapper.set_permission_mode_request_str(desired);
                        let request_id = request["request_id"].as_str().unwrap_or_default().to_owned();
                        if write_line(&mut stdin, &request).await.is_err() {
                            mapper.pending_permission_modes.remove(&request_id);
                            fail_unsent_turn(&mut mapper, &event_tx, "permission_mode_switch",
                                format!("Claude could not request permission mode {desired}")).await;
                        } else {
                            pending_mode_turn = Some(PendingModeTurn {
                                command, request_id, mode: desired.to_owned(),
                                deadline: std::time::Instant::now() + std::time::Duration::from_secs(60),
                            });
                        }
                        continue;
                    }
                }
                if matches!(&command, SessionCommand::Shutdown) {
                    if let Some(pending) = pending_mode_turn.take() {
                        mapper.pending_permission_modes.remove(&pending.request_id);
                        fail_unsent_turn(&mut mapper, &event_tx, "permission_mode_switch",
                            "Claude session closed before permission mode was confirmed".into()).await;
                    }
                }
                let mode_option = matches!(&command, SessionCommand::SetOption { id, .. } if id == "permissionMode");
                if let ControlFlow::Break(reason) = handle_command(
                    command,
                    &mut mapper,
                    goal.as_mut(),
                    &mut stdin,
                    &event_tx,
                    &mut child,
                )
                .await
                {
                    break Some(reason.into());
                }
                if mode_option {
                    for id in mapper.pending_permission_modes.keys() {
                        pending_option_modes.entry(id.clone()).or_insert_with(||
                            std::time::Instant::now() + std::time::Duration::from_secs(60));
                    }
                }
            }
            Sel::Cmd(None) => {
                if let Some(pending) = pending_mode_turn.take() {
                    mapper.pending_permission_modes.remove(&pending.request_id);
                    fail_unsent_turn(&mut mapper, &event_tx, "permission_mode_switch",
                        "Claude session closed before permission mode was confirmed".into()).await;
                }
                // UI dropped the command sender: tear down.
                let _ = stdin.close().await;
                let _ = child.kill();
                break Some("command channel closed".into());
            }
            Sel::Line(Some(line)) => {
                let confirmation = pending_mode_turn.as_ref().and_then(|pending| {
                    let msg: Value = serde_json::from_str(&line).ok()?;
                    if msg["type"] != "control_response" || msg["response"]["request_id"] != pending.request_id {
                        return None;
                    }
                    Some(if msg["response"]["subtype"] == "success" {
                        Ok(())
                    } else {
                        Err(msg["response"]["error"].as_str().unwrap_or("Claude rejected the permission-mode request").to_owned())
                    })
                });
                let events = match serde_json::from_str::<Value>(&line) {
                    Ok(msg) => mapper.on_wire_message(msg, claude_dir.as_deref()).await,
                    Err(e) => {
                        log::debug!("claude: non-JSON stdout line ({e}): {line}");
                        Vec::new()
                    }
                };
                pending_option_modes.retain(|id, _| mapper.pending_permission_modes.contains_key(id));
                for ev in events {
                    if event_tx.send(ev).await.is_err() {
                        let _ = child.kill();
                        return;
                    }
                }
                for write in mapper.take_outgoing() {
                    let _ = write_line(&mut stdin, &write).await;
                }
                if let Some(confirmation) = confirmation
                    && let Some(pending) = pending_mode_turn.take()
                {
                    match confirmation {
                        Ok(()) if mapper.applied_permission_mode == pending.mode => {
                            if let ControlFlow::Break(reason) = handle_command(
                                pending.command, &mut mapper, goal.as_mut(), &mut stdin, &event_tx, &mut child,
                            ).await {
                                break Some(reason.into());
                            }
                        }
                        result => {
                            let detail = result.err().unwrap_or_else(|| format!(
                                "Claude confirmed permission mode {}, expected {}",
                                mapper.applied_permission_mode, pending.mode,
                            ));
                            fail_unsent_turn(&mut mapper, &event_tx, "permission_mode_switch",
                                format!("Claude could not switch permission mode to {}: {detail}", pending.mode)).await;
                        }
                    }
                }
                if let (Some(root), Some(session_id)) = (&claude_dir, mapper.session_id.as_deref())
                {
                    if goal
                        .as_ref()
                        .is_none_or(|reader| reader.session_id() != session_id)
                    {
                        goal = Some(GoalTranscript::new(
                            root.clone(),
                            session_id.to_owned(),
                            config.resumed,
                        ));
                    }
                    let now = std::time::Instant::now();
                    let next = goal_poll.map_or(now, |(next, _)| next);
                    goal_poll = Some((next, now + GOAL_POLL_WINDOW));
                }
                process_tail_requests(
                    mapper.take_tail_requests(),
                    &mut tailers,
                    claude_dir.as_deref(),
                    &tail_tx,
                )
                .await;
            }
            Sel::Line(None) => {
                if let Some(pending) = pending_mode_turn.take() {
                    mapper.pending_permission_modes.remove(&pending.request_id);
                    fail_unsent_turn(&mut mapper, &event_tx, "permission_mode_switch",
                        "Claude closed before permission mode was confirmed".into()).await;
                }
                // stdout closed: child is exiting. Status is read before our
                // kill below so the child's own exit code is not masked by it.
                provider_exited = true;
                break Some(match child.try_status().ok().flatten() {
                    Some(status) => format!("claude exited with {status}"),
                    None => "claude closed stdout".into(),
                });
            }
        }
    };

    let _ = stdin.close().await;
    for control in tailers.into_values() {
        let _ = control.send(TailControl::Stop).await;
    }
    // Tail acknowledgements no longer reach this loop; release the held
    // terminal snapshots so the parent rows settle before the close.
    for ev in mapper.take_pending_subagent_terminals() {
        let _ = event_tx.send(ev).await;
    }
    let _ = child.kill();
    let _ = child.status().await;
    // The child is gone, so its stderr pipe normally closes and the drain task
    // finishes; the timeout covers a grandchild keeping the pipe open, which
    // would otherwise hang the close event forever.
    smol::future::or(stderr_task, async {
        smol::Timer::after(std::time::Duration::from_millis(500)).await;
    })
    .await;
    let closed_reason = closed_reason.map(|reason| {
        if provider_exited {
            stderr_tail.append_to(reason, "\nstderr:\n")
        } else {
            reason
        }
    });
    let _ = event_tx
        .send(AgentEvent::SessionClosed {
            reason: closed_reason,
        })
        .await;
}

enum Sel {
    GoalRead,
    ModeTimeout,
    Cmd(Option<SessionCommand>),
    Line(Option<String>),
    Tail(Option<SubagentTailNotice>),
}

struct PendingModeTurn {
    command: SessionCommand,
    request_id: String,
    mode: String,
    deadline: std::time::Instant,
}

async fn fail_unsent_turn(
    mapper: &mut Mapper,
    event_tx: &smol::channel::Sender<AgentEvent>,
    code: &str,
    message: String,
) {
    let turn_id = mapper.start_turn();
    mapper.current_turn_id = None;
    let _ = event_tx.send(AgentEvent::TurnStarted { turn_id: turn_id.clone() }).await;
    let _ = event_tx.send(AgentEvent::TurnFailed { code: code.to_owned(), message }).await;
    let _ = event_tx.send(AgentEvent::TurnCompleted { turn_id, status: TurnStatus::Failed, usage: None }).await;
}

/// One read of a tailed subagent transcript, handed back to the actor so the
/// mapper stays the single writer of the canonical stream: it dedupes the
/// child items against the stdout feed and, on `stopped`, releases the
/// Subagent item's terminal snapshot after them.
pub(crate) struct SubagentTailNotice {
    pub(crate) parent_id: String,
    pub(crate) events: Vec<AgentEvent>,
    pub(crate) model: Option<(Option<String>, Option<String>)>,
    /// Final batch: the tail acknowledged Stop (or lost its actor).
    pub(crate) stopped: bool,
}

pub(crate) enum TailControl {
    PreferPath(PathBuf),
    Stop,
}

async fn process_tail_requests(
    requests: Vec<TailRequest>,
    tailers: &mut HashMap<String, smol::channel::Sender<TailControl>>,
    claude_dir: Option<&Path>,
    tail_tx: &smol::channel::Sender<SubagentTailNotice>,
) {
    for request in requests {
        match request {
            TailRequest::Start {
                parent_id,
                task_id,
                session_id,
            } => {
                if tailers.contains_key(&parent_id) {
                    continue;
                }
                let (control_tx, control_rx) = smol::channel::unbounded();
                tailers.insert(parent_id.clone(), control_tx);
                let claude_dir = claude_dir.map(Path::to_path_buf);
                let notices = tail_tx.clone();
                smol::spawn(run_subagent_tail(
                    parent_id, task_id, session_id, claude_dir, control_rx, notices,
                ))
                .detach();
            }
            TailRequest::PreferPath { parent_id, path } => {
                if let Some(control) = tailers.get(&parent_id) {
                    let _ = control.send(TailControl::PreferPath(path)).await;
                }
            }
            TailRequest::Stop { parent_id } => {
                if let Some(control) = tailers.get(&parent_id) {
                    let _ = control.send(TailControl::Stop).await;
                }
            }
        }
    }
}

/// Poll a subagent's transcript until Stop; the final read is flushed in the
/// notice that carries `stopped`, so the actor sees every child item before
/// it releases the Subagent item's terminal snapshot.
pub(crate) async fn run_subagent_tail(
    parent_id: String,
    task_id: String,
    session_id: String,
    claude_dir: Option<PathBuf>,
    controls: smol::channel::Receiver<TailControl>,
    notices: smol::channel::Sender<SubagentTailNotice>,
) {
    let mut reader: Option<crate::subagent_tail::TailReader> = None;
    let mut stopping = false;
    let mut woken = None;
    loop {
        for control in woken
            .take()
            .into_iter()
            .chain(std::iter::from_fn(|| controls.try_recv().ok()))
        {
            match control {
                // The notification's output file duplicates a transcript that
                // discovery already tails; switching would replay it from byte 0.
                TailControl::PreferPath(preferred) if reader.is_none() => {
                    reader = Some(crate::subagent_tail::TailReader::new(
                        preferred,
                        parent_id.clone(),
                    ));
                }
                TailControl::PreferPath(_) => {}
                TailControl::Stop => stopping = true,
            }
        }
        if reader.is_none()
            && let Some(root) = &claude_dir
            && let Some(found) =
                crate::subagent_tail::find_transcript(root, &session_id, &task_id, &parent_id)
        {
            reader = Some(crate::subagent_tail::TailReader::new(
                found,
                parent_id.clone(),
            ));
        }
        let mut notice = SubagentTailNotice {
            parent_id: parent_id.clone(),
            events: Vec::new(),
            model: None,
            stopped: stopping,
        };
        if let Some(reader) = &mut reader {
            match reader.read_appended() {
                Ok(mapped) => {
                    notice.events = mapped;
                    notice.model = reader.take_model();
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => log::debug!("claude subagent tail read failed: {err}"),
            }
        }
        if (stopping || !notice.events.is_empty() || notice.model.is_some())
            && notices.send(notice).await.is_err()
        {
            return;
        }
        if stopping {
            return;
        }
        // Wake on the next control so a Stop flushes immediately instead of
        // after a poll interval; a closed channel means the actor is gone.
        woken = smol::future::or(
            async { Some(controls.recv().await.unwrap_or(TailControl::Stop)) },
            async {
                smol::Timer::after(std::time::Duration::from_millis(400)).await;
                None
            },
        )
        .await;
    }
}

/// Run one of the CLI's slash commands the way stream-json input does: as a
/// user message that opens a turn.
async fn slash_command_turn(
    command: &str,
    mapper: &mut Mapper,
    stdin: &mut smol::process::ChildStdin,
    event_tx: &smol::channel::Sender<AgentEvent>,
) -> ControlFlow<&'static str> {
    if write_line(stdin, &user_message(command, &[]))
        .await
        .is_err()
    {
        let _ = event_tx
            .send(AgentEvent::Error {
                message: format!("failed to write {command} to provider stdin"),
                fatal: true,
            })
            .await;
        return ControlFlow::Break("provider stdin write failed");
    }
    let turn_id = mapper.start_turn();
    let _ = event_tx.send(AgentEvent::TurnStarted { turn_id }).await;
    ControlFlow::Continue(())
}

async fn handle_command(
    command: SessionCommand,
    mapper: &mut Mapper,
    goal: Option<&mut GoalTranscript>,
    stdin: &mut smol::process::ChildStdin,
    event_tx: &smol::channel::Sender<AgentEvent>,
    child: &mut smol::process::Child,
) -> ControlFlow<&'static str> {
    match command {
        SessionCommand::SendTurn {
            delivery_id,
            text,
            options: _,
            attachments,
        } => {
            let msg = user_message(&text, &attachments);
            if write_turn_message(stdin, &msg, delivery_id, event_tx)
                .await
                .is_err()
            {
                let _ = event_tx
                    .send(AgentEvent::Error {
                        message: "failed to write turn to provider stdin".into(),
                        fatal: true,
                    })
                    .await;
                return ControlFlow::Break("provider stdin write failed");
            }
            let turn_id = mapper.start_turn();
            let _ = event_tx.send(AgentEvent::TurnStarted { turn_id }).await;
            ControlFlow::Continue(())
        }
        SessionCommand::Interrupt => {
            let msg = mapper.interrupt_request();
            let _ = write_line(stdin, &msg).await;
            ControlFlow::Continue(())
        }
        // The CLI runs its `/compact` slash command from a stream-json user
        // message as a turn of its own: `status: compacting`, then
        // `compact_boundary` (trigger `manual`) and a `result`.
        SessionCommand::Compact => slash_command_turn("/compact", mapper, stdin, event_tx).await,
        // `/goal` runs as a turn of its own too; with a new goal the CLI asks
        // the model to start on it in that turn.
        SessionCommand::SetGoal {
            objective: Some(objective),
            status: None | Some(GoalStatus::Active),
        } => slash_command_turn(&format!("/goal {objective}"), mapper, stdin, event_tx).await,
        // Pausing is not offered: `Caps::goals` has no pause for Claude Code.
        SessionCommand::SetGoal { .. } => ControlFlow::Continue(()),
        // A met or impossible goal is already gone from the CLI; only the
        // record of it is left to dismiss.
        SessionCommand::ClearGoal
            if goal
                .as_ref()
                .and_then(|reader| reader.goal())
                .is_some_and(|goal| {
                    matches!(goal.status, GoalStatus::Complete | GoalStatus::Blocked)
                }) =>
        {
            if let Some(reader) = goal {
                reader.dismiss();
            }
            let _ = event_tx.send(AgentEvent::GoalUpdated { goal: None }).await;
            ControlFlow::Continue(())
        }
        SessionCommand::ClearGoal => {
            slash_command_turn("/goal clear", mapper, stdin, event_tx).await
        }
        SessionCommand::RespondApproval {
            request_id,
            decision,
        } => {
            let response = mapper.build_approval_response(&request_id, decision.clone());
            resolve_response(
                stdin,
                event_tx,
                request_id,
                decision,
                response,
                |request_id, decision| AgentEvent::ApprovalResolved {
                    request_id,
                    decision,
                },
            )
            .await;
            ControlFlow::Continue(())
        }
        SessionCommand::RespondUserInput {
            request_id,
            answers,
        } => {
            let response = mapper.build_user_input_response(&request_id, &answers);
            resolve_response(
                stdin,
                event_tx,
                request_id,
                answers,
                response,
                |request_id, answers| AgentEvent::UserInputResolved {
                    request_id,
                    answers,
                },
            )
            .await;
            ControlFlow::Continue(())
        }
        SessionCommand::SetOption{id,value}if id=="muteki:operation"=>{
            let request_id=value["request_id"].as_str().unwrap_or("").to_owned();let name=value["name"].as_str().unwrap_or("");
            if !value["arguments"].as_str().unwrap_or("").is_empty() || mapper.current_turn_id.is_some() || !matches!(name,"context"|"mcp"){
                let _=event_tx.send(AgentEvent::RuntimeOperationResult{request_id,result:None,error:Some("Claude operation is unavailable or accepts no arguments".into())}).await;return ControlFlow::Continue(());
            }
            let message=mapper.control_request(json!({"subtype":if name=="context"{"get_context_usage"}else{"mcp_status"}}));let native=message["request_id"].as_str().unwrap_or("").to_owned();mapper.pending_operations.insert(native,request_id.clone());
            if let Err(error)=write_line(stdin,&message).await{let _=event_tx.send(AgentEvent::RuntimeOperationResult{request_id,result:None,error:Some(error.to_string())}).await;}ControlFlow::Continue(())
        }
        SessionCommand::SetOption { id, value } if id == "permissionMode" => {
            let Some(mode) = value.as_str() else {
                return ControlFlow::Continue(());
            };
            let msg = mapper.set_permission_mode_request_str(mode);
            if write_line(stdin, &msg).await.is_err() {
                if let Some(request_id) = msg.get("request_id").and_then(Value::as_str) {
                    mapper.pending_permission_modes.remove(request_id);
                }
                let _ = event_tx
                    .send(AgentEvent::Warning {
                        message: format!(
                            "claude: failed to write permission-mode switch for {mode}"
                        ),
                    })
                    .await;
            }
            ControlFlow::Continue(())
        }
        SessionCommand::SetOption { .. } => ControlFlow::Continue(()),
        SessionCommand::Rewind {
            checkpoint_id,
            mode,
        } => {
            match mapper.begin_rewind(checkpoint_id.clone(), mode) {
                Ok(request) if write_line(stdin, &request).await.is_ok() => {}
                Ok(_) => {
                    let _ = event_tx
                        .send(AgentEvent::RewindFailed {
                            checkpoint_id,
                            mode,
                            failure: RewindFailure::new(RewindFailureReason::ProcessGone, Some("failed to write rewind request to provider stdin".into())),
                        })
                        .await;
                }
                Err(error) => {
                    let _ = event_tx
                        .send(AgentEvent::RewindFailed {
                            checkpoint_id,
                            mode,
                            failure: RewindFailure::new(RewindFailureReason::Unsupported, Some(error)),
                        })
                        .await;
                }
            }
            ControlFlow::Continue(())
        }
        SessionCommand::Steer {
            request_id,
            text,
            attachments,
        } => {
            // Steering writes the *same* stream-json user-message line as
            // `SendTurn`, but deliberately skips the turn bookkeeping: no
            // `start_turn()`, no `TurnStarted`. The CLI folds the message into
            // the turn that is already running at its next input checkpoint.
            // A successful write only queues the request id; acceptance is
            // emitted when stdout next reports `status: requesting`, the best
            // available signal that the CLI consumed it. There is a small
            // residual race: a steer written microseconds before that status
            // may actually miss the request, but the CLI protocol exposes no
            // stronger acknowledgement, so we accept it at that checkpoint.
            let msg = user_message(&text, &attachments);
            write_steering_message(stdin, &msg, request_id, mapper, event_tx).await;
            ControlFlow::Continue(())
        }
        SessionCommand::PreviewRewind { checkpoint_id, mode } => {
            let reason = if mode == RewindMode::Conversation {
                None
            } else {
                Some("Claude Code 文件回退将还原该检查点之后的所有修改".into())
            };
            let _ = event_tx
                .send(AgentEvent::RewindPreviewed {
                    checkpoint_id,
                    mode,
                    files: None,
                    reason,
                })
                .await;
            ControlFlow::Continue(())
        }
        SessionCommand::UndoRewind { checkpoint_id, .. } => {
            let _ = event_tx
                .send(AgentEvent::RewindUndoFailed {
                    checkpoint_id,
                    failure: RewindFailure::new(
                        RewindFailureReason::Unsupported,
                        Some("Claude Code keeps no rewind undoable".into()),
                    ),
                })
                .await;
            ControlFlow::Continue(())
        }
        SessionCommand::StopBackgroundTask { task_id } => {
            if let Some(id) = task_id {
                let req = serde_json::json!({
                    "type": "control_request",
                    "request": { "subtype": "stop_task", "task_id": id }
                });
                let _ = write_line(stdin, &req).await;
            }
            ControlFlow::Continue(())
        }
        SessionCommand::RetryTurn { .. } => ControlFlow::Continue(()),
        SessionCommand::Shutdown => {
            // Settle pending AskUserQuestion callbacks before closing stdin.
            for (request_id, response) in mapper.cancel_pending_user_input() {
                let _ = write_line(stdin, &response).await;
                let _ = event_tx
                    .send(AgentEvent::UserInputResolved {
                        request_id,
                        answers: serde_json::Map::new(),
                    })
                    .await;
            }
            let _ = stdin.close().await;
            let _ = child.kill();
            ControlFlow::Break("shutdown requested")
        }
    }
}

async fn resolve_response<T>(
    stdin: &mut smol::process::ChildStdin,
    event_tx: &smol::channel::Sender<AgentEvent>,
    request_id: String,
    value: T,
    response: Option<Value>,
    resolved: impl FnOnce(String, T) -> AgentEvent,
) {
    if let Some(response) = response {
        match write_line(stdin, &response).await {
            Ok(())=>{let _=event_tx.send(resolved(request_id,value)).await;},
            Err(error)=>{let _=event_tx.send(AgentEvent::ControlDelivery{request_id,operation:"response".into(),status:"unknown".into(),error:Some(json!({"code":"runtime.delivery_unknown","message":error.to_string()}))}).await;}
        }
    } else {
        let _=event_tx.send(AgentEvent::ControlDelivery{request_id,operation:"response".into(),status:"failed".into(),error:Some(json!({"code":"runtime.request_closed","message":"Native request already closed"}))}).await;
    }
}

fn control_response(request_id: &str, body: Value) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": body,
        }
    })
}

async fn write_turn_message<W: AsyncWrite + Unpin>(
    stdin: &mut W,
    message: &Value,
    delivery_id: u64,
    event_tx: &smol::channel::Sender<AgentEvent>,
) -> std::io::Result<()> {
    write_line(stdin, message).await?;
    // The complete newline-delimited prompt is now flushed. Emit acceptance
    // before returning to the actor loop, where stdout EOF can win the race.
    let _ = event_tx
        .send(AgentEvent::TurnAccepted { delivery_id })
        .await;
    Ok(())
}

async fn write_steering_message<W: AsyncWrite + Unpin>(
    stdin: &mut W,
    message: &Value,
    request_id: String,
    mapper: &mut Mapper,
    event_tx: &smol::channel::Sender<AgentEvent>,
) {
    if write_line(stdin, message).await.is_err() {
        let _ = event_tx
            .send(AgentEvent::Error {
                message: "failed to write steering message to provider stdin".into(),
                fatal: true,
            })
            .await;
    } else {
        mapper.pending_steers.push_back(request_id);
    }
}

async fn write_line<W: AsyncWrite + Unpin>(stdin: &mut W, value: &Value) -> std::io::Result<()> {
    let mut line = serde_json::to_string(value).unwrap_or_default();
    line.push('\n');
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await
}

/// Build a stream-json user message line. Text comes first, followed by one
/// `image` content block per attachment (`source: {type: "base64", media_type,
/// data}` — the Anthropic content-block shape the CLI accepts).
fn user_message(text: &str, attachments: &[Attachment]) -> Value {
    let mut content = vec![json!({ "type": "text", "text": text })];
    for attachment in attachments {
        if let Some(path) = attachment.workspace_file() {
            content.push(json!({
                "type": "document",
                "title": "Workspace file reference",
                "source": {"type":"text","media_type":"text/plain","data":path}
            }));
            continue;
        }
        content.push(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": attachment.media_type,
                "data": attachment.data_base64,
            }
        }));
    }
    json!({
        "type": "user",
        "session_id": "",
        "parent_tool_use_id": null,
        "message": {
            "role": "user",
            "content": content,
        }
    })
}

/// Remembers what kind of tool-use item a `tool_use_id` refers to, so that when
/// the matching `tool_result` arrives we can emit the right `ItemCompleted`.
enum ToolItem {
    Command {
        command: String,
        output: String,
    },
    File {
        changes: Vec<FileChange>,
    },
    Tool {
        name: String,
        input: Value,
    },
    Subagent {
        agent_type: String,
        description: String,
        status: ItemStatus,
        summary: Option<String>,
        model: Option<String>,
        effort: Option<String>,
        /// The subagent this one was spawned from, for nested spawns.
        parent_item_id: Option<String>,
    },
}

impl ToolItem {
    fn subagent(content: &ItemContent, parent_item_id: Option<String>) -> Option<Self> {
        let ItemContent::Subagent {
            agent_type,
            description,
            status,
            summary,
            model,
            effort,
        } = content
        else {
            return None;
        };
        Some(ToolItem::Subagent {
            agent_type: agent_type.clone(),
            description: description.clone(),
            status: *status,
            summary: summary.clone(),
            model: model.clone(),
            effort: effort.clone(),
            parent_item_id,
        })
    }
}

enum TailRequest {
    Start {
        parent_id: String,
        task_id: String,
        session_id: String,
    },
    PreferPath {
        parent_id: String,
        path: PathBuf,
    },
    Stop {
        parent_id: String,
    },
}

struct PendingApproval {
    input: Value,
    suggestions: HashMap<String, Value>,
}

/// Where a child item's lifecycle stands, as far as the canonical stream has
/// been told. An update keeps its snapshot so only a changed one passes.
enum ChildItemStage {
    Started,
    Updated(ItemContent),
    Completed,
}

struct PendingRewind {
    checkpoint_id: String,
    mode: RewindMode,
    conversation: bool,
    files: Option<RewindFiles>,
    files_failure: Option<RewindFailure>,
}

fn served_model_changed(expected: &str, served: &str) -> bool {
    // CLI-generated errors/refusals do not identify a serving model.
    !served.is_empty()
        && served != "<synthetic>"
        && !strip_context_window_suffix(expected)
            .eq_ignore_ascii_case(strip_context_window_suffix(served))
}

#[derive(Default)]
struct AssistantMessageState {
    blocks_seen: usize,
    model: Option<String>,
}

pub(crate) struct Mapper {
    session_id: Option<String>,
    current_message_id: Option<String>,
    /// How many content blocks of each streamed message we have already seen in
    /// an `assistant` line. The CLI splits one message across several `assistant`
    /// lines — one per block, each carrying a *one-element* `content` array — so
    /// enumerating that array gives 0 every time, while the `content_block_delta`
    /// stream numbers the blocks 0, 1, 2… The completed item has to reuse the
    /// stream's numbering or the timeline shows the same text twice: once from
    /// the deltas (`msg:1`) and once from the completion (`msg:0`).
    assistant_messages: HashMap<String, AssistantMessageState>,
    turn_counter: usize,
    current_turn_id: Option<String>,
    last_completed_turn:Option<String>,
    item_origins:HashMap<String,String>,
    /// The next replayed top-level user message carries this turn's provider
    /// checkpoint UUID. Steering messages do not arm this flag.
    awaiting_turn_checkpoint: bool,
    control_counter: usize,
    tool_items: HashMap<String, ToolItem>,
    task_tools: HashMap<String, String>,
    child_mappers: HashMap<String, crate::subagent_tail::TranscriptMapper>,
    tail_requests: Vec<TailRequest>,
    /// Subagents whose transcript tail has not yet acknowledged its Stop. A
    /// terminal Subagent snapshot is held in `pending_subagent_terminals` until
    /// then, so the mirror's turn closes after the tail's final child items.
    tailed: HashSet<String>,
    pending_subagent_terminals: HashMap<String, AgentEvent>,
    /// The last lifecycle stage each child item reached through either feed
    /// (stdout `parent_tool_use_id` lines or the transcript tail); the other
    /// feed's copy of the same stage is dropped and a stale start cannot
    /// reopen a completed item.
    child_items: HashMap<String, ChildItemStage>,
    pending_approvals: HashMap<String, PendingApproval>,
    /// Pending `AskUserQuestion` prompts: control request_id → the original
    /// `questions` array, echoed back verbatim in the allow response.
    pending_user_input: HashMap<String, Value>,
    /// Set when we send an `interrupt` control_request; the next non-success
    /// `result` is then attributed to the interrupt rather than a failure
    /// (the CLI's result carries no reliable interrupt marker).
    interrupt_pending: bool,
    /// Permission mode currently applied on the CLI, so we only switch on change.
    applied_permission_mode: String,
    auto_unavailable: Option<String>,
    /// Live permission-mode requests awaiting Claude's correlated response.
    /// A successful stdin write is not proof that the CLI applied the mode.
    pending_permission_modes: HashMap<String, String>,
    outgoing: Vec<Value>,
    request_usage: Value,
    latest_usage: TokenUsage,
    usage_message_id: Option<String>,
    usage_message_ids: HashSet<String>,
    result_ids: HashSet<String>,
    /// Successfully written steers awaiting the CLI's next input checkpoint.
    pending_steers: VecDeque<String>,
    /// Once the CLI exposes request checkpoints, never use the legacy fallback.
    saw_requesting: bool,
    /// Tasks still owned by this Claude process, whatever their `task_type`:
    /// a Monitor watch or a backgrounded subagent keeps the process busy
    /// exactly like a background Bash command. When the list becomes empty
    /// Claude immediately re-invokes itself with the completion notification,
    /// so zero is not published until that follow-up result.
    background_tasks: HashSet<String>,
    /// Native rewind control requests awaiting Claude's correlated response.
    pending_rewinds: HashMap<String, PendingRewind>,
    pending_operations:HashMap<String,String>,
    native_rewind: bool,
    /// Last assistant model published to the canonical event stream.
    last_served_model: Option<String>,
    /// Selected model: the comparison baseline and the source for synthetic refusals.
    expected_model: Option<String>,
    /// Whether a served-model mismatch was already reported for the active turn.
    fallback_detected: bool,
    /// Latest structured stop reason for the active turn.
    stop_reason: Option<String>,
    /// Classifier category captured from the active turn's structured refusal details.
    pending_refusal_category: Option<ClassifierCategory>,
    /// Reset time reported for a rejected usage window in the active turn.
    limit_resets_at: Option<u64>,
    /// Warning-bearing stop reason already emitted for this occurrence.
    warned_stop_reason: Option<String>,
}

impl Mapper {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::new_configured("default".to_owned(), false, None)
    }

    fn new_configured(
        applied_permission_mode: String,
        native_rewind: bool,
        expected_model: Option<String>,
    ) -> Self {
        Mapper {
            session_id: None,
            current_message_id: None,
            assistant_messages: HashMap::new(),
            turn_counter: 0,
            current_turn_id: None,
            last_completed_turn:None,
            item_origins:HashMap::new(),
            awaiting_turn_checkpoint: false,
            control_counter: 0,
            tool_items: HashMap::new(),
            task_tools: HashMap::new(),
            child_mappers: HashMap::new(),
            tail_requests: Vec::new(),
            tailed: HashSet::new(),
            pending_subagent_terminals: HashMap::new(),
            child_items: HashMap::new(),
            pending_approvals: HashMap::new(),
            pending_user_input: HashMap::new(),
            interrupt_pending: false,
            applied_permission_mode,
            auto_unavailable: None,
            pending_permission_modes: HashMap::new(),
            outgoing: Vec::new(),
            request_usage: json!({}),
            latest_usage: TokenUsage::default(),
            usage_message_id: None,
            usage_message_ids: HashSet::new(),
            result_ids: HashSet::new(),
            pending_steers: VecDeque::new(),
            saw_requesting: false,
            background_tasks: HashSet::new(),
            pending_rewinds: HashMap::new(),
            pending_operations:HashMap::new(),
            native_rewind,
            last_served_model: None,
            expected_model,
            fallback_detected: false,
            stop_reason: None,
            pending_refusal_category: None,
            limit_resets_at: None,
            warned_stop_reason: None,
        }
    }

    /// Drain queued control-response writes for the actor to send.
    fn take_outgoing(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.outgoing)
    }

    fn take_tail_requests(&mut self) -> Vec<TailRequest> {
        std::mem::take(&mut self.tail_requests)
    }

    /// Allocate the next synthesized turn id and mark it in-flight.
    fn start_turn(&mut self) -> String {
        self.current_message_id = None;
        self.usage_message_id = None;
        self.request_usage = json!({});
        if self.latest_usage.freshness == crate::ContextFreshness::Current {
            self.latest_usage.freshness = crate::ContextFreshness::LastKnown;
        }
        self.turn_counter += 1;
        let id = format!("turn-{}", self.turn_counter);
        self.current_turn_id = Some(id.clone());
        self.awaiting_turn_checkpoint = self.native_rewind;
        self.fallback_detected = false;
        self.stop_reason = None;
        self.pending_refusal_category = None;
        self.warned_stop_reason = None;
        id
    }

    fn next_control_id(&mut self) -> String {
        self.control_counter += 1;
        format!("tcode-ctrl-{}", self.control_counter)
    }

    fn begin_rewind(&mut self, checkpoint_id: String, mode: RewindMode) -> Result<Value, String> {
        if !self.native_rewind {
            return Err("this Claude Code version does not expose native rewind controls".into());
        }
        Ok(self.rewind_request(PendingRewind {
            checkpoint_id, mode, conversation: !mode.includes_files(), files: None, files_failure: None,
        }))
    }

    fn rewind_request(&mut self, pending: PendingRewind) -> Value {
        let request = if pending.conversation {
            json!({
                "subtype": "rewind_conversation",
                "target_message_uuid": pending.checkpoint_id,
                "interrupt_if_running": true,
                "allow_summarized_target": true,
            })
        } else {
            json!({ "subtype": "rewind_files", "user_message_id": pending.checkpoint_id })
        };
        let message = self.control_request(request);
        let request_id = message["request_id"].as_str().unwrap_or_default().to_owned();
        self.pending_rewinds.insert(request_id, pending);
        message
    }

    fn control_request(&mut self, request: Value) -> Value {
        json!({
            "type": "control_request",
            "request_id": self.next_control_id(),
            "request": request,
        })
    }

    /// Client → CLI interrupt control request.
    fn interrupt_request(&mut self) -> Value {
        if self.current_turn_id.is_some() {
            self.interrupt_pending = true;
        }
        self.control_request(json!({ "subtype": "interrupt" }))
    }

    /// Client → CLI `set_permission_mode` control request. Wire shape verified
    /// against `@anthropic-ai/claude-agent-sdk` v0.3.170 (`browser-sdk.js`):
    /// `request(e)` wraps the payload as
    /// `{request_id, type:"control_request", request:e}`, and
    /// `setPermissionMode(m)` sends `{subtype:"set_permission_mode", mode:m}`.
    fn set_permission_mode_request_str(&mut self, mode: &str) -> Value {
        let message = self.control_request(json!({
            "subtype": "set_permission_mode",
            "mode": mode,
        }));
        let request_id = message["request_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        self.pending_permission_modes
            .insert(request_id, mode.to_owned());
        message
    }

    /// Build the `control_response` answering a pending `can_use_tool` prompt.
    fn build_approval_response(
        &mut self,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> Option<Value> {
        let pending = self.pending_approvals.get(request_id)?;
        let response = match decision {
            ApprovalDecision::Option(id) if id == "allow" => json!({
                "behavior": "allow", "updatedInput": pending.input,
            }),
            ApprovalDecision::Option(id) if id == "deny" => json!({
                "behavior": "deny", "message": "User declined tool execution.",
            }),
            ApprovalDecision::Option(id) => {
                let suggestions = pending.suggestions.get(&id)?;
                json!({ "behavior": "allow", "updatedInput": pending.input, "updatedPermissions": suggestions })
            }
            ApprovalDecision::Cancel => json!({
                "behavior": "deny", "message": "User cancelled tool execution.",
            }),
        };
        self.pending_approvals.remove(request_id);
        Some(control_response(request_id, response))
    }

    /// Build the `control_response` allowing a pending `AskUserQuestion` prompt,
    /// echoing the original `questions` alongside the collected `answers`.
    /// Returns `None` for an unknown request id.
    fn build_user_input_response(
        &mut self,
        request_id: &str,
        answers: &serde_json::Map<String, Value>,
    ) -> Option<Value> {
        let questions = self.pending_user_input.remove(request_id)?;
        Some(control_response(
            request_id,
            json!({
                "behavior": "allow",
                "updatedInput": { "questions": questions, "answers": answers }
            }),
        ))
    }

    /// Drain every pending `AskUserQuestion`, producing `(request_id, deny
    /// control_response)` pairs with the cancellation message.
    fn cancel_pending_user_input(&mut self) -> Vec<(String, Value)> {
        self.pending_user_input
            .drain()
            .map(|(request_id, _questions)| {
                let response = control_response(
                    &request_id,
                    json!({
                        "behavior": "deny",
                        "message": "User cancelled tool execution.",
                    }),
                );
                (request_id, response)
            })
            .collect()
    }

    async fn on_wire_message(&mut self, msg: Value, claude_dir: Option<&Path>) -> Vec<AgentEvent> {
        let before=self.current_turn_id.clone().or_else(||self.last_completed_turn.clone());
        let wire_agent=msg["parent_tool_use_id"].as_str().map(str::to_owned);
        let wire_parent=msg["parent_tool_use_id"].as_str().or_else(||msg["request"]["tool_use_id"].as_str()).map(str::to_owned);
        let resumed = msg
            .pointer("/tool_use_result/resumedAgentId")
            .and_then(Value::as_str);
        let task_id = resumed.or_else(|| {
            (msg.get("type").and_then(Value::as_str) == Some("system"))
                .then(|| msg.get("task_id").and_then(Value::as_str))
                .flatten()
        });
        let session_id = msg
            .get("session_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .or(self.session_id.as_deref())
            .unwrap_or_default()
            .to_owned();
        if let Some(task_id) = task_id
            && self.task_tools.get(task_id).is_none_or(|id| {
                !matches!(self.tool_items.get(id), Some(ToolItem::Subagent { .. }))
            })
            && let Some(root) = claude_dir
            && !session_id.is_empty()
        {
            // A resumed child's notification names SendMessage, while its
            // transcript still names the original Agent call, even after a
            // parent process restart. Claude's metadata preserves that identity.
            let root = root.to_owned();
            let session = session_id.clone();
            let task = task_id.to_owned();
            if let Some(item) = crate::process::unblock(move || {
                crate::subagent_tail::read_subagent_spawn(&root, &session, &task)
            })
            .await
            {
                self.task_tools.insert(task_id.to_owned(), item.id.clone());
                if let Some(tool) = ToolItem::subagent(&item.content, item.parent_item_id) {
                    self.tool_items.entry(item.id).or_insert(tool);
                }
            }
        }
        let resumed = resumed.and_then(|task_id| self.task_tools.get(task_id).cloned());
        let mut events = self.on_message(msg);
        if let Some(parent_id) = resumed {
            events.extend(self.update_subagent(&parent_id, ItemStatus::InProgress, None));
        }
        events.into_iter().map(|mut event|{
            if matches!(event,AgentEvent::TurnStarted{..}|AgentEvent::ContinuationStarted{..}){return event;}
            let (item,parent)=match &event{AgentEvent::ItemStarted(item)|AgentEvent::ItemUpdated(item)|AgentEvent::ItemCompleted(item)=>(Some(item.id.clone()),item.parent_item_id.clone()),AgentEvent::Delta{item_id,..}=>(Some(item_id.clone()),None),AgentEvent::TurnCompleted{turn_id,..}=>(None,Some(turn_id.clone())),_=>(None,None)};
            let origin=item.as_ref().and_then(|id|self.item_origins.get(id)).or_else(||parent.as_ref().and_then(|id|self.item_origins.get(id))).or_else(||wire_parent.as_ref().and_then(|id|self.item_origins.get(id))).cloned().or_else(||match &event{AgentEvent::TurnCompleted{turn_id,..}=>Some(turn_id.clone()),_=>self.current_turn_id.clone().or(before.clone())});
            if let Some(origin)=origin{if let Some(item)=item{self.item_origins.entry(item).or_insert_with(||origin.clone());}if let AgentEvent::ApprovalRequested(request)=&mut event{request.turn_id=Some(origin.clone());}AgentEvent::Origin{turn_id:origin,agent_id:wire_agent.clone(),event:Box::new(event)}}else{event}
        }).collect()
    }

    /// Map one CLI stdout message to zero or more outcomes.
    pub(crate) fn on_message(&mut self, msg: Value) -> Vec<AgentEvent> {
        if let Some(parent_id) = msg
            .get("parent_tool_use_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            let mut events = match crate::subagent_tail::model_and_effort(&msg) {
                Some((model, effort)) => self.note_subagent_model(parent_id, model, effort),
                None => Vec::new(),
            };
            let child = self
                .child_mappers
                .entry(parent_id.to_owned())
                .or_insert_with(|| crate::subagent_tail::TranscriptMapper::new(parent_id))
                .map_value(&msg);
            events.extend(self.fold_child_events(child));
            return events;
        }
        match msg.get("type").and_then(Value::as_str) {
            Some("system") => self.on_system(&msg),
            Some("stream_event") => {
                let mut events = self.synthesized_turn_started();
                events.extend(self.on_stream_event(&msg));
                events
            }
            Some("assistant") => {
                let mut events = self.synthesized_turn_started();
                let steer_events = if self.saw_requesting {
                    Vec::new()
                } else {
                    self.accept_pending_steers()
                };
                events.extend(steer_events);
                events.extend(self.on_assistant(&msg));
                events
            }
            Some("user") => self.on_user(&msg),
            Some("control_request") => self.on_control_request(&msg),
            Some("control_response") => self.on_control_response(&msg),
            Some("rate_limit_event") => self.on_rate_limit_event(&msg),
            Some("result") => self.on_result(&msg),
            other => {
                log::debug!("claude: ignoring message type {other:?}");
                Vec::new()
            }
        }
    }

    fn permission_options(&self) -> AgentEvent {
        let mut descriptor =
            crate::permission_control(ProviderKind::ClaudeCode).expect("Claude permission control");
        if let OptionDescriptor::Select { options, .. } = &mut descriptor {
            if let Some(auto) = options.iter_mut().find(|option| option.value == "auto") {
                auto.unavailable.clone_from(&self.auto_unavailable);
            }
            if !options
                .iter()
                .any(|option| option.value == self.applied_permission_mode)
            {
                options.push(crate::SelectOption {
                    value: self.applied_permission_mode.clone(),
                    label: self.applied_permission_mode.clone(),
                    description: None,
                    unavailable: Some(if self.applied_permission_mode == "plan" {
                        "Entered by the agent".into()
                    } else {
                        "Reported by Claude".into()
                    }),
                });
            }
        }
        AgentEvent::ProviderOptions {
            descriptors: vec![descriptor],
            selections: vec![OptionSelection {
                id: "permissionMode".into(),
                value: json!(self.applied_permission_mode),
            }],
        }
    }

    fn on_rate_limit_event(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let info = msg.get("rate_limit_info");
        self.limit_resets_at = info
            .filter(|info| info.get("status").and_then(Value::as_str) == Some("rejected"))
            .and_then(|info| info.get("resetsAt"))
            .and_then(Value::as_u64);
        Vec::new()
    }

    fn synthesized_turn_started(&mut self) -> Vec<AgentEvent> {
        if self.session_id.is_some() && self.current_turn_id.is_none() {
            let turn_id = self.start_turn();
            self.awaiting_turn_checkpoint = false;
            let mut events=Vec::new();if let Some(origin)=self.last_completed_turn.clone(){events.push(AgentEvent::ContinuationStarted{origin_turn_id:origin,turn_id:turn_id.clone()});}events.push(AgentEvent::TurnStarted { turn_id });events
        } else {
            Vec::new()
        }
    }

    fn on_system(&mut self, msg: &Value) -> Vec<AgentEvent> {
        if msg.get("subtype").and_then(Value::as_str) == Some("status")
            && msg.get("status").and_then(Value::as_str) == Some("requesting")
        {
            self.saw_requesting = true;
            return self.accept_pending_steers();
        }
        match msg.get("subtype").and_then(Value::as_str) {
            Some("init") => {}
            // Claude compacted its context window (verified shape:
            // `{type:"system", subtype:"compact_boundary", compact_metadata:{…}}`).
            Some("compact_boundary") => {
                self.request_usage = json!({});
                self.usage_message_id = None;
                self.current_message_id = None;
                self.latest_usage.used_tokens = None;
                self.latest_usage.input_tokens = None;
                self.latest_usage.cached_input_tokens = None;
                self.latest_usage.output_tokens = None;
                self.latest_usage.freshness = crate::ContextFreshness::AwaitingObservation;
                return vec![AgentEvent::ContextCompacted(crate::Compaction {
                    in_progress: false,
                    trigger: msg
                        .pointer("/compact_metadata/trigger")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    pre_tokens: msg
                        .pointer("/compact_metadata/pre_tokens")
                        .and_then(Value::as_u64),
                    post_tokens: msg
                        .pointer("/compact_metadata/post_tokens")
                        .and_then(Value::as_u64),
                    dropped_tokens: msg
                        .pointer("/compact_metadata/cumulative_dropped_tokens")
                        .and_then(Value::as_u64),
                    duration_ms: msg
                        .pointer("/compact_metadata/duration_ms")
                        .and_then(Value::as_u64),
                    ..Default::default()
                })];
            }
            Some("status") if msg.get("status").and_then(Value::as_str) == Some("compacting") => {
                self.latest_usage.freshness = crate::ContextFreshness::Compacting;
                return vec![AgentEvent::ContextCompacted(crate::Compaction {
                    in_progress: true,
                    trigger: None,
                    pre_tokens: None,
                    ..Default::default()
                })];
            }
            Some("task_started") => return self.on_task_started(msg),
            Some("task_updated") => return self.on_task_updated(msg),
            Some("task_notification") => return self.on_task_notification(msg),
            Some("background_tasks_changed") => return self.on_background_tasks_changed(msg),
            Some("model_refusal_fallback") => return self.on_model_refusal_fallback(msg),
            Some("api_retry") => return vec![api_retry_notice(msg)],
            other => {
                log::debug!("claude: ignoring system/{other:?}");
                return Vec::new();
            }
        }
        if let Some(mode) = msg.get("permissionMode").and_then(Value::as_str) {
            if mode == "auto" {
                self.auto_unavailable = None;
            } else if self.session_id.is_none() && self.applied_permission_mode == "auto" {
                self.auto_unavailable =
                    Some("Not available: the session started in another mode".into());
            }
            self.applied_permission_mode = mode.to_owned();
        }
        if self.session_id.is_some() {
            return vec![self.permission_options()];
        }
        let session_id = match msg.get("session_id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => return Vec::new(),
        };
        self.session_id = Some(session_id.clone());
        let model = msg.get("model").and_then(Value::as_str).map(str::to_string);
        let mut events = vec![
            AgentEvent::SessionStarted {
                provider_session_id: session_id.clone(),
                resume: ResumeCursor(json!({ "session_id": session_id })),
                model,
            },
            self.permission_options(),
        ];
        // The `slash_commands` (Command) and `skills` (Skill) arrays feed the
        // composer's `/` and `$` menus. Both are arrays of names (no descriptions).
        let commands = parse_provider_commands(msg);
        if !commands.is_empty() {
            events.push(AgentEvent::ProviderCommands { commands });
        }
        events
    }

    fn on_model_refusal_fallback(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let (Some(expected), Some(actual)) = (
            msg.get("original_model").and_then(Value::as_str),
            msg.get("fallback_model").and_then(Value::as_str),
        ) else {
            log::debug!("claude: ignoring malformed model_refusal_fallback");
            return Vec::new();
        };
        if !served_model_changed(expected, actual) {
            return Vec::new();
        }
        self.fallback_detected = true;
        let mut events = vec![AgentEvent::ModelFallbackDetected {
            expected: expected.to_owned(),
            actual: actual.to_owned(),
            category: msg
                .get("api_refusal_category")
                .and_then(Value::as_str)
                .map(ClassifierCategory::parse),
            checkpoint_id: msg
                .get("refused_user_message_uuid")
                .and_then(Value::as_str)
                .map(str::to_owned),
            parent_tool_use_id: msg
                .get("parent_tool_use_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }];
        events.extend(self.observe_served_model(&json!({ "model": actual })));
        events
    }

    fn accept_pending_steers(&mut self) -> Vec<AgentEvent> {
        self.pending_steers
            .drain(..)
            .map(|request_id| AgentEvent::SteerAccepted { request_id })
            .collect()
    }

    fn on_task_started(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let Some(tool_use_id) = msg.get("tool_use_id").and_then(Value::as_str) else {
            return Vec::new();
        };
        let mut parent_id = tool_use_id.to_owned();
        if let Some(task_id) = msg.get("task_id").and_then(Value::as_str) {
            parent_id = self
                .task_tools
                .entry(task_id.to_owned())
                .or_insert(parent_id)
                .clone();
            if msg.get("task_type").and_then(Value::as_str) != Some("local_bash") {
                self.tailed.insert(parent_id.clone());
                self.tail_requests.push(TailRequest::Start {
                    parent_id: parent_id.clone(),
                    task_id: task_id.to_owned(),
                    session_id: msg
                        .get("session_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                });
            }
        }
        self.update_subagent(&parent_id, ItemStatus::InProgress, None)
    }

    fn on_background_tasks_changed(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let raw_tasks = msg
            .get("tasks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let current = raw_tasks
            .iter()
            .filter_map(|task| task.get("task_id").and_then(Value::as_str))
            .map(str::to_owned)
            .collect::<HashSet<_>>();
        let task_list = raw_tasks
            .iter()
            .filter_map(|task| {
                let id = task.get("task_id").and_then(Value::as_str)?.to_owned();
                Some(crate::BackgroundTask {
                    name: task
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or(&id)
                        .to_owned(),
                    kind: match task.get("task_type").and_then(Value::as_str) {
                        Some("local_bash") => crate::BackgroundKind::Command,
                        Some("local_agent" | "remote_agent" | "in_process_teammate") => {
                            crate::BackgroundKind::Agent
                        }
                        _ => crate::BackgroundKind::Other,
                    },
                    item_id: self.task_tools.get(&id).cloned(),
                    started_at: now,
                    started_reported: false,
                    id,
                })
            })
            .collect::<Vec<_>>();
        let had_background_tasks = !self.background_tasks.is_empty();
        self.background_tasks = current;
        let list_event = AgentEvent::BackgroundTaskList { tasks: task_list };
        if self.background_tasks.is_empty() && had_background_tasks {
            // `background_tasks_changed []` arrives next to the final
            // task_notification (recorded traces have it on either side) and
            // before the provider's self-invoked follow-up turn. Keep the last
            // non-zero liveness until that turn's result so the runtime cannot
            // park/restart the process in this handoff window.
            vec![list_event]
        } else {
            vec![
                list_event,
                AgentEvent::BackgroundTasksChanged {
                    count: self.background_tasks.len(),
                },
            ]
        }
    }

    fn on_task_updated(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let Some(task_id) = msg.get("task_id").and_then(Value::as_str) else {
            return Vec::new();
        };
        let Some(tool_use_id) = self.task_tools.get(task_id).cloned() else {
            return Vec::new();
        };
        let Some(status) = msg.pointer("/patch/status").and_then(Value::as_str) else {
            return Vec::new();
        };
        let status = subagent_status(status);
        if status != ItemStatus::InProgress {
            self.tail_requests.push(TailRequest::Stop {
                parent_id: tool_use_id.clone(),
            });
        }
        self.update_subagent(&tool_use_id, status, None)
    }

    fn on_task_notification(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let task_id = msg
            .get("task_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let tool_use_id = task_id
            .as_ref()
            .and_then(|task_id| self.task_tools.get(task_id).cloned())
            .or_else(|| {
                msg.get("tool_use_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        let Some(tool_use_id) = tool_use_id else {
            return Vec::new();
        };
        // The task stays in `background_tasks` until the CLI's own
        // `background_tasks_changed` drops it: removing it here lets a drain
        // that follows this notification publish zero before the follow-up
        // turn (see `on_background_tasks_changed`).
        if let Some(path) = msg.get("output_file").and_then(Value::as_str) {
            self.tail_requests.push(TailRequest::PreferPath {
                parent_id: tool_use_id.clone(),
                path: PathBuf::from(path),
            });
        }
        self.tail_requests.push(TailRequest::Stop {
            parent_id: tool_use_id.clone(),
        });
        let status = subagent_status(
            msg.get("status")
                .and_then(Value::as_str)
                .unwrap_or("completed"),
        );
        let summary = msg
            .get("summary")
            .and_then(Value::as_str)
            .map(one_line_summary);
        // A Command still held here is a background Bash: foreground ones
        // leave `tool_items` with their tool_result.
        if matches!(
            self.tool_items.get(&tool_use_id),
            Some(ToolItem::Command { .. })
        ) {
            self.complete_background_command(&tool_use_id, status, summary)
        } else {
            self.update_subagent(&tool_use_id, status, summary)
        }
    }

    fn complete_background_command(
        &mut self,
        tool_use_id: &str,
        status: ItemStatus,
        summary: Option<String>,
    ) -> Vec<AgentEvent> {
        let Some(ToolItem::Command {
            command,
            mut output,
        }) = self.tool_items.remove(tool_use_id)
        else {
            return Vec::new();
        };
        if output.trim().is_empty()
            && let Some(summary) = summary
        {
            output = summary;
        }
        vec![AgentEvent::ItemCompleted(ThreadItem {
            id: tool_use_id.to_owned(),
            parent_item_id: None,
            content: ItemContent::CommandExecution {
                command,
                output,
                exit_code: (status == ItemStatus::Completed).then_some(0),
                status,
            },
        })]
    }

    fn update_subagent(
        &mut self,
        tool_use_id: &str,
        status: ItemStatus,
        summary: Option<String>,
    ) -> Vec<AgentEvent> {
        let Some(ToolItem::Subagent {
            agent_type,
            description,
            status: saved_status,
            summary: saved_summary,
            model,
            effort,
            parent_item_id,
        }) = self.tool_items.get_mut(tool_use_id)
        else {
            return Vec::new();
        };
        if summary.is_some() {
            *saved_summary = summary;
        }
        *saved_status = status;
        let event = AgentEvent::ItemUpdated(ThreadItem {
            id: tool_use_id.to_owned(),
            parent_item_id: parent_item_id.clone(),
            content: ItemContent::Subagent {
                agent_type: agent_type.clone(),
                description: description.clone(),
                status,
                summary: saved_summary.clone(),
                model: model.clone(),
                effort: effort.clone(),
            },
        });
        if status == ItemStatus::InProgress {
            vec![event]
        } else {
            self.gate_subagent_terminal(tool_use_id, event)
        }
    }

    /// Hold a terminal Subagent snapshot while its transcript tail is still
    /// flushing; the tail's Stop acknowledgement releases the latest one.
    fn gate_subagent_terminal(&mut self, tool_use_id: &str, event: AgentEvent) -> Vec<AgentEvent> {
        if self.tailed.contains(tool_use_id) {
            self.pending_subagent_terminals
                .insert(tool_use_id.to_owned(), event);
            Vec::new()
        } else {
            vec![event]
        }
    }

    /// Fold one feed's child items into the canonical stream: drop the stage
    /// the other feed already delivered, and give a nested spawn the same
    /// `tool_items` lifecycle as a top-level one so its task events and tail
    /// settle it.
    fn fold_child_events(&mut self, events: Vec<AgentEvent>) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        for event in events {
            if !self.advance_child_item(&event) {
                continue;
            }
            match &event {
                AgentEvent::ItemStarted(item)
                    if matches!(item.content, ItemContent::Subagent { .. }) =>
                {
                    if let Some(tool) =
                        ToolItem::subagent(&item.content, item.parent_item_id.clone())
                    {
                        self.tool_items.entry(item.id.clone()).or_insert(tool);
                    }
                    out.push(event);
                }
                AgentEvent::ItemCompleted(item)
                    if matches!(item.content, ItemContent::Subagent { .. })
                        && matches!(
                            self.tool_items.get(&item.id),
                            Some(ToolItem::Subagent { .. })
                        ) =>
                {
                    let id = item.id.clone();
                    self.tool_items.remove(&id);
                    out.extend(self.gate_subagent_terminal(&id, event));
                }
                _ => out.push(event),
            }
        }
        out
    }

    /// Record a child item's stage; false when this stage was already delivered.
    fn advance_child_item(&mut self, event: &AgentEvent) -> bool {
        let (item, stage) = match event {
            AgentEvent::ItemStarted(item) => (item, ChildItemStage::Started),
            AgentEvent::ItemUpdated(item) => (item, ChildItemStage::Updated(item.content.clone())),
            AgentEvent::ItemCompleted(item) => (item, ChildItemStage::Completed),
            _ => return true,
        };
        match (self.child_items.get(&item.id), &stage) {
            (Some(ChildItemStage::Completed), _) | (Some(_), ChildItemStage::Started) => false,
            (Some(ChildItemStage::Updated(previous)), ChildItemStage::Updated(content))
                if previous == content =>
            {
                false
            }
            _ => {
                self.child_items.insert(item.id.clone(), stage);
                true
            }
        }
    }

    /// Fold one batch from a subagent transcript tail: its child items, the
    /// model it observed, and — once it has stopped — the terminal Subagent
    /// snapshot that was waiting on its final flush.
    pub(crate) fn on_tail_notice(&mut self, notice: SubagentTailNotice) -> Vec<AgentEvent> {
        let mut events = match notice.model {
            Some((model, effort)) => self.note_subagent_model(&notice.parent_id, model, effort),
            None => Vec::new(),
        };
        events.extend(self.fold_child_events(notice.events));
        if notice.stopped {
            self.tailed.remove(&notice.parent_id);
            events.extend(self.pending_subagent_terminals.remove(&notice.parent_id));
        }
        events
    }

    /// Terminal snapshots whose tail acknowledgement will never arrive
    /// (session teardown).
    fn take_pending_subagent_terminals(&mut self) -> Vec<AgentEvent> {
        self.tailed.clear();
        std::mem::take(&mut self.pending_subagent_terminals)
            .into_values()
            .collect()
    }

    /// Record the model/effort a child transcript reports for its spawn item
    /// and re-emit the item when either changed.
    pub(crate) fn note_subagent_model(
        &mut self,
        tool_use_id: &str,
        model: Option<String>,
        effort: Option<String>,
    ) -> Vec<AgentEvent> {
        let Some(ToolItem::Subagent {
            status,
            model: saved_model,
            effort: saved_effort,
            ..
        }) = self.tool_items.get_mut(tool_use_id)
        else {
            return Vec::new();
        };
        let model = model.or_else(|| saved_model.clone());
        let effort = effort.or_else(|| saved_effort.clone());
        if model == *saved_model && effort == *saved_effort {
            return Vec::new();
        }
        *saved_model = model;
        *saved_effort = effort;
        let status = *status;
        self.update_subagent(tool_use_id, status, None)
    }

    fn on_stream_event(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let event = match msg.get("event") {
            Some(e) => e,
            None => return Vec::new(),
        };
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                self.current_message_id = event
                    .get("message")
                    .and_then(|m| m.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let message = &event["message"];
                let mut events = self.observe_served_model(message);
                events.extend(self.observe_request_usage(
                    message.get("id").and_then(Value::as_str),
                    message.get("usage"),
                    true,
                ));
                events
            }
            // Claude 5 models stream a thinking block whose deltas carry no
            // text, so the block's start is the only sign the model is
            // thinking: a placeholder reasoning item renders as "Thinking…"
            // until text or the next block arrives.
            Some("content_block_start")
                if event.pointer("/content_block/type").and_then(Value::as_str)
                    == Some("thinking") =>
            {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                vec![AgentEvent::ItemStarted(ThreadItem {
                    id: self.block_item_id(index),
                    parent_item_id: None,
                    content: ItemContent::Reasoning {
                        text: String::new(),
                    },
                })]
            }
            Some("content_block_delta") => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                let delta = match event.get("delta") {
                    Some(d) => d,
                    None => return Vec::new(),
                };
                let (kind, text) = match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => (
                        DeltaKind::AssistantText,
                        delta.get("text").and_then(Value::as_str),
                    ),
                    Some("thinking_delta") => (
                        DeltaKind::ReasoningText,
                        delta
                            .get("thinking")
                            .and_then(Value::as_str)
                            .or_else(|| delta.get("text").and_then(Value::as_str)),
                    ),
                    // input_json_delta and friends: tool input is reconstructed
                    // from the (complete) `assistant` message instead.
                    _ => return Vec::new(),
                };
                let text = match text {
                    Some(t) if !t.is_empty() => t,
                    _ => return Vec::new(),
                };
                vec![AgentEvent::Delta {
                    item_id: self.block_item_id(index),
                    kind,
                    text: text.to_string(),
                }]
            }
            Some("message_delta") => {
                let mut events = self.observe_stop_reason(
                    event.pointer("/delta/stop_reason").and_then(Value::as_str),
                );
                if let Some(usage) = event.get("usage") {
                    let id = self.current_message_id.clone();
                    events.extend(self.observe_request_usage(id.as_deref(), Some(usage), false));
                }
                events
            }
            _ => Vec::new(),
        }
    }

    /// Stream deltas are cumulative fields within one request, never increments.
    /// Assistant messages can repeat an ID for parallel tool blocks.
    fn observe_request_usage(
        &mut self,
        id: Option<&str>,
        usage: Option<&Value>,
        start: bool,
    ) -> Vec<AgentEvent> {
        let Some(id) = id else {
            return Vec::new();
        };
        if self.usage_message_id.as_deref() != Some(id) {
            if !self.usage_message_ids.insert(id.to_owned()) {
                return Vec::new();
            }
            self.usage_message_id = Some(id.to_owned());
            self.request_usage = json!({});
        }
        if let Some(fields) = usage.and_then(Value::as_object) {
            for key in [
                "input_tokens",
                "cache_read_input_tokens",
                "cache_creation_input_tokens",
                "output_tokens",
            ] {
                if let Some(value) = fields.get(key).filter(|v| v.is_u64()) {
                    // Non-streaming assistant output is a placeholder; never regress
                    // the cumulative output already observed in the stream.
                    if key != "output_tokens" || value.as_u64() >= self.request_usage[key].as_u64()
                    {
                        self.request_usage[key] = value.clone();
                    }
                }
            }
        }
        self.latest_usage.turn_processed_tokens = None;
        let mapped = map_usage(&self.request_usage, None);
        self.latest_usage.input_tokens = mapped.input_tokens;
        self.latest_usage.cached_input_tokens = mapped.cached_input_tokens;
        self.latest_usage.output_tokens = mapped.output_tokens;
        // Claude's context observation counts the latest request's input and caches.
        // Output deltas describe generated traffic, not a new input observation.
        self.latest_usage.used_tokens = mapped.input_tokens.map(|input| {
            input
                .saturating_add(mapped.cached_input_tokens.unwrap_or(0))
                .saturating_add(
                    self.request_usage["cache_creation_input_tokens"]
                        .as_u64()
                        .unwrap_or(0),
                )
        });
        self.latest_usage.freshness = if self.latest_usage.used_tokens.is_some() {
            crate::ContextFreshness::Current
        } else {
            crate::ContextFreshness::AwaitingObservation
        };
        if usage.is_some() || start {
            vec![AgentEvent::TokenUsage(self.latest_usage),AgentEvent::UsageDetail{scope:"message".into(),usage_id:id.into(),usage:json!({"input_tokens":mapped.input_tokens,"output_tokens":mapped.output_tokens,"cache_read_tokens":mapped.cached_input_tokens,"cache_write_tokens":mapped.cache_write_tokens,"input_includes_cache":false})}]
        } else {
            Vec::new()
        }
    }

    fn block_item_id(&self, index: u64) -> String {
        match &self.current_message_id {
            Some(id) => format!("{id}:{index}"),
            None => format!("msg:{index}"),
        }
    }

    fn observe_fallback_block(&mut self, block: &Value) -> Vec<AgentEvent> {
        let (Some(expected), Some(actual)) = (
            block.pointer("/from/model").and_then(Value::as_str),
            block.pointer("/to/model").and_then(Value::as_str),
        ) else {
            return Vec::new();
        };
        let mut events = Vec::new();
        if !self.fallback_detected && served_model_changed(expected, actual) {
            self.fallback_detected = true;
            events.push(AgentEvent::ModelFallbackDetected {
                expected: expected.to_owned(),
                actual: actual.to_owned(),
                category: None,
                checkpoint_id: None,
                parent_tool_use_id: None,
            });
        }
        events.extend(self.observe_served_model(&block["to"]));
        events
    }

    fn observe_served_model(&mut self, message: &Value) -> Vec<AgentEvent> {
        let Some(model) = message
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty() && *model != "<synthetic>")
        else {
            return Vec::new();
        };
        let mut events = Vec::new();
        if !self.fallback_detected
            && let Some(expected) = self.expected_model.as_deref()
            && served_model_changed(expected, model)
        {
            self.fallback_detected = true;
            events.push(AgentEvent::ModelFallbackDetected {
                expected: expected.to_owned(),
                actual: model.to_owned(),
                category: None,
                checkpoint_id: None,
                // Child messages are routed to their own mapper by on_message.
                parent_tool_use_id: None,
            });
        }
        if self.last_served_model.as_deref() != Some(model) {
            self.last_served_model = Some(model.to_owned());
            events.push(AgentEvent::ServedModel {
                model: model.to_owned(),
                reason: None,
            });
        }
        events
    }

    fn on_assistant(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let message = match msg.get("message") {
            Some(m) => m,
            None => return Vec::new(),
        };
        let mut out = self.observe_request_usage(
            message.get("id").and_then(Value::as_str),
            message.get("usage"),
            false,
        );
        if message
            .pointer("/stop_details/type")
            .and_then(Value::as_str)
            == Some("refusal")
        {
            self.pending_refusal_category = message
                .pointer("/stop_details/category")
                .and_then(Value::as_str)
                .map(ClassifierCategory::parse);
        }
        let msg_id = message
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("msg")
            .to_string();
        let state = self.assistant_messages.entry(msg_id.clone()).or_default();
        let model = message.get("model").and_then(Value::as_str);
        let model_changed = state.model.as_deref() != model;
        if model_changed {
            state.model = model.map(str::to_owned);
        }
        let content = message.get("content").and_then(Value::as_array);
        let first_index = state.blocks_seen;
        state.blocks_seen += content.map_or(0, Vec::len);
        // Split assistant blocks retain the message_start model even after a
        // fallback boundary; observing an unchanged header would undo the switch.
        if model_changed {
            out.extend(self.observe_served_model(message));
        }
        out.extend(self.observe_stop_reason(message.get("stop_reason").and_then(Value::as_str)));
        let Some(content) = content else { return out };

        for (offset, block) in content.iter().enumerate() {
            let index = first_index + offset;
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                    out.push(AgentEvent::ItemCompleted(ThreadItem {
                        id: format!("{msg_id}:{index}"),
                        parent_item_id: None,
                        content: ItemContent::AssistantMessage {
                            text: text.to_string(),
                        },
                    }));
                }
                Some("thinking") => {
                    let text = block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .or_else(|| block.get("text").and_then(Value::as_str))
                        .unwrap_or("");
                    // The CLI redacts thinking content in this line even when it
                    // streamed the deltas; completing with "" would blank the
                    // reasoning the user just watched arrive.
                    if !text.is_empty() {
                        out.push(AgentEvent::ItemCompleted(ThreadItem {
                            id: format!("{msg_id}:{index}"),
                            parent_item_id: None,
                            content: ItemContent::Reasoning {
                                text: text.to_string(),
                            },
                        }));
                    }
                }
                Some("tool_use") => {
                    out.extend(self.on_tool_use(block));
                }
                Some("fallback") => out.extend(self.observe_fallback_block(block)),
                _ => {}
            }
        }
        out
    }

    fn observe_stop_reason(&mut self, reason: Option<&str>) -> Vec<AgentEvent> {
        let Some(reason) = reason.filter(|reason| !reason.is_empty()) else {
            return Vec::new();
        };
        self.stop_reason = Some(reason.to_owned());
        let message = match reason {
            "refusal" => Some("Request declined by safety classifiers (stop_reason: refusal)"),
            "max_tokens" => Some("Response truncated: max_tokens limit reached"),
            _ => None,
        };
        if message.is_none() || self.warned_stop_reason.as_deref() == Some(reason) {
            return Vec::new();
        }
        self.warned_stop_reason = Some(reason.to_owned());
        vec![AgentEvent::Warning {
            message: message.unwrap().to_owned(),
        }]
    }

    fn on_tool_use(&mut self, block: &Value) -> Vec<AgentEvent> {
        let tool_use_id = match block.get("id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => return Vec::new(),
        };
        let name = block
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let input = block.get("input").cloned().unwrap_or_else(|| json!({}));

        // TodoWrite drives the structured plan/task sidebar, not the timeline.
        if is_todo_tool(&name) {
            if let Some(steps) = extract_plan_steps_from_todo(&input) {
                return vec![AgentEvent::PlanUpdated {
                    turn_id: self.current_turn_id.clone(),
                    explanation: None,
                    steps,
                }];
            }
            return Vec::new();
        }

        let (item, content) = if is_agent_tool(&name.to_lowercase()) {
            let content = spawned_subagent(&input);
            let item = ToolItem::subagent(&content, None).expect("a spawn snapshot is a Subagent");
            (item, content)
        } else if name == "Bash" {
            let command = input
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            (
                ToolItem::Command {
                    command: command.clone(),
                    output: String::new(),
                },
                ItemContent::CommandExecution {
                    command,
                    output: String::new(),
                    exit_code: None,
                    status: ItemStatus::InProgress,
                },
            )
        } else if is_file_tool(&name) {
            let changes = file_changes(&name, &input);
            (
                ToolItem::File {
                    changes: changes.clone(),
                },
                ItemContent::FileChange {
                    changes,
                    status: ItemStatus::InProgress,
                },
            )
        } else {
            (
                ToolItem::Tool {
                    name: name.clone(),
                    input: input.clone(),
                },
                ItemContent::ToolCall {
                    presentation: None,
                    image_reads: Vec::new(),
                    name,
                    input,
                    output: None,
                    status: ItemStatus::InProgress,
                },
            )
        };

        self.tool_items.insert(tool_use_id.clone(), item);
        vec![AgentEvent::ItemStarted(ThreadItem {
            id: tool_use_id,
            parent_item_id: None,
            content,
        })]
    }

    fn on_user(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let content = msg
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array);
        let mut out = Vec::new();
        let has_tool_result = content.is_some_and(|content| {
            content
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        });
        if self.awaiting_turn_checkpoint
            && !has_tool_result
            && let (Some(turn_id), Some(checkpoint_id)) = (
                self.current_turn_id.clone(),
                msg.get("uuid").and_then(Value::as_str),
            )
        {
            self.awaiting_turn_checkpoint = false;
            out.push(AgentEvent::TurnCheckpoint {
                turn_id,
                checkpoint_id: checkpoint_id.to_owned(),
            });
        }
        for block in content.into_iter().flatten() {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let tool_use_id = match block.get("tool_use_id").and_then(Value::as_str) {
                Some(id) => id.to_string(),
                None => continue,
            };
            let launched_in_background = msg.pointer("/tool_use_result/backgroundTaskId").is_some();
            let item = match self.tool_items.remove(&tool_use_id) {
                Some(i) => i,
                None => continue,
            };
            // A background Agent's tool_result only acknowledges the launch;
            // the subagent keeps running until its task_notification.
            if matches!(item, ToolItem::Subagent { .. })
                && msg
                    .pointer("/tool_use_result/status")
                    .and_then(Value::as_str)
                    == Some("async_launched")
            {
                if let Some(agent_id) = msg
                    .pointer("/tool_use_result/agentId")
                    .and_then(Value::as_str)
                {
                    self.task_tools
                        .insert(agent_id.to_owned(), tool_use_id.clone());
                }
                self.tool_items.insert(tool_use_id.clone(), item);
                out.extend(self.update_subagent(&tool_use_id, ItemStatus::InProgress, None));
                continue;
            }
            let is_error = block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let output = tool_result_text(block.get("content"));
            let status = if is_error {
                ItemStatus::Failed
            } else {
                ItemStatus::Completed
            };
            let mut parent_item_id = None;
            let content = match item {
                ToolItem::Command { command, .. } if launched_in_background => {
                    self.tool_items.insert(
                        tool_use_id.clone(),
                        ToolItem::Command {
                            command: command.clone(),
                            output: output.clone(),
                        },
                    );
                    ItemContent::CommandExecution {
                        command,
                        output,
                        exit_code: None,
                        status: ItemStatus::InProgress,
                    }
                }
                ToolItem::Command { command, .. } => ItemContent::CommandExecution {
                    command,
                    output,
                    exit_code: if is_error { Some(1) } else { Some(0) },
                    status,
                },
                ToolItem::File { mut changes } => {
                    if let Some(structured_patch) = msg.pointer("/tool_use_result/structuredPatch")
                    {
                        let diff = render_structured_patch(structured_patch);
                        let path = msg
                            .pointer("/tool_use_result/filePath")
                            .and_then(Value::as_str);
                        for change in &mut changes {
                            if let Some(path) = path {
                                change.path = path.to_owned();
                            }
                            // Successful writes can report `structuredPatch: []`;
                            // that omission must not erase the input-derived diff.
                            if let Some(diff) = &diff {
                                change.diff = Some(diff.clone());
                            }
                        }
                    }
                    ItemContent::FileChange { changes, status }
                }
                ToolItem::Tool { name, input } => ItemContent::ToolCall {
                    presentation: None,
                    image_reads: if !is_error && crate::image_reads::is_read_tool(&name) {
                        crate::image_reads::content_images(
                            block.get("content").unwrap_or(&Value::Null),
                        )
                    } else {
                        Vec::new()
                    },
                    name,
                    input,
                    output: Some(match block.get("content").and_then(Value::as_array) {
                        Some(blocks) if blocks.iter().any(|block| matches!(block["type"].as_str(), Some("image" | "resource" | "resource_link"))) => Value::Array(blocks.clone()).to_string(),
                        _ => output,
                    }),
                    status,
                },
                ToolItem::Subagent {
                    agent_type,
                    description,
                    summary,
                    model,
                    effort,
                    parent_item_id: parent,
                    ..
                } => {
                    parent_item_id = parent;
                    ItemContent::Subagent {
                        agent_type,
                        description,
                        status,
                        summary: summary.or_else(|| {
                            (!output.trim().is_empty()).then(|| one_line_summary(&output))
                        }),
                        model,
                        effort,
                    }
                }
            };
            let event = if matches!(
                &content,
                ItemContent::CommandExecution {
                    status: ItemStatus::InProgress,
                    ..
                }
            ) {
                AgentEvent::ItemUpdated
            } else {
                AgentEvent::ItemCompleted
            };
            let is_subagent = matches!(content, ItemContent::Subagent { .. });
            let event = event(ThreadItem {
                id: tool_use_id.clone(),
                parent_item_id,
                content,
            });
            if is_subagent {
                out.extend(self.gate_subagent_terminal(&tool_use_id, event));
            } else {
                out.push(event);
            }
        }
        out
    }

    fn on_control_response(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let Some(response) = msg.get("response") else {
            return Vec::new();
        };
        let Some(request_id) = response.get("request_id").and_then(Value::as_str) else {
            return Vec::new();
        };
        if let Some(request_id)=self.pending_operations.remove(request_id){return vec![AgentEvent::RuntimeOperationResult{request_id,result:(response["subtype"]=="success").then(||response["response"].clone()),error:(response["subtype"]!="success").then(||response["error"].as_str().unwrap_or("Claude rejected the native operation").into())}];}
        if let Some(mode) = self.pending_permission_modes.remove(request_id) {
            if response.get("subtype").and_then(Value::as_str) == Some("success") {
                self.applied_permission_mode = response
                    .pointer("/response/permissionMode")
                    .or_else(|| response.pointer("/response/mode"))
                    .and_then(Value::as_str)
                    .unwrap_or(&mode)
                    .to_owned();
                if self.applied_permission_mode == "auto" {
                    self.auto_unavailable = None;
                }
                return vec![self.permission_options()];
            }
            let error = response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("Claude Code rejected the permission-mode request");
            let warning = AgentEvent::Warning {
                message: format!("claude: failed to switch permission mode: {error}"),
            };
            if mode == "auto"
                && response
                    .get("error_code")
                    .and_then(Value::as_str)
                    .is_some_and(|code| code.starts_with("auto_mode_"))
            {
                self.auto_unavailable = Some(error.to_owned());
                return vec![warning, self.permission_options()];
            }
            return vec![warning];
        }
        let Some(mut pending) = self.pending_rewinds.remove(request_id) else { return Vec::new(); };
        let succeeded = response.get("subtype").and_then(Value::as_str) == Some("success");
        let result = response.get("response").unwrap_or(&Value::Null);
        let error = || response.get("error").or_else(||result.get("error")).and_then(Value::as_str).map(str::to_owned);
        if !pending.conversation {
            if succeeded && result.get("canRewind").and_then(Value::as_bool) != Some(false) {
                pending.files = Some(rewind_files(result));
            } else {
                pending.files_failure = Some(files_failure(error()));
            }
            if pending.mode.includes_conversation() {
                pending.conversation = true;
                let request = self.rewind_request(pending);
                self.outgoing.push(request);
                return Vec::new();
            }
            return vec![match pending.files_failure {
                Some(failure) => AgentEvent::RewindFailed { checkpoint_id: pending.checkpoint_id, mode: pending.mode, failure },
                None => AgentEvent::RewindCompleted {
                    checkpoint_id: pending.checkpoint_id, mode: pending.mode, prefill: None,
                    prefill_attachments: Vec::new(), later_history: LaterHistory::Truncated,
                    files: pending.files, incomplete: None, undo_token: None,
                },
            }];
        }
        if succeeded && result.get("rewound").and_then(Value::as_bool) == Some(true) {
            let restored = if pending.files_failure.is_some() { RewindMode::Conversation } else { pending.mode };
            return vec![AgentEvent::RewindCompleted {
                checkpoint_id: pending.checkpoint_id, mode: restored,
                prefill: result.get("prefillText").and_then(Value::as_str).filter(|text| !text.is_empty()).map(str::to_owned),
                prefill_attachments: Vec::new(), later_history: LaterHistory::Truncated, files: pending.files,
                incomplete: pending.files_failure.map(|failure| RewindIncomplete { requested: pending.mode, failure }),
                undo_token: None,
            }];
        }
        let failure = RewindFailure::new(if succeeded { conversation_refusal(result.get("reason").and_then(Value::as_str)) } else { RewindFailureReason::Other }, error());
        if pending.mode.includes_files() && pending.files_failure.is_none() {
            vec![AgentEvent::RewindCompleted {
                checkpoint_id: pending.checkpoint_id, mode: RewindMode::Files, prefill: None,
                prefill_attachments: Vec::new(), later_history: LaterHistory::Truncated, files: pending.files,
                incomplete: Some(RewindIncomplete { requested: pending.mode, failure }), undo_token: None,
            }]
        } else {
            vec![AgentEvent::RewindFailed { checkpoint_id: pending.checkpoint_id, mode: pending.mode, failure }]
        }
    }

    fn on_control_request(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let request = match msg.get("request") {
            Some(r) => r,
            None => return Vec::new(),
        };
        if request.get("subtype").and_then(Value::as_str) != Some("can_use_tool") {
            log::debug!(
                "claude: ignoring control_request/{:?}",
                request.get("subtype").and_then(Value::as_str)
            );
            return Vec::new();
        }
        let request_id = match msg.get("request_id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => return Vec::new(),
        };
        let tool_name = request
            .get("tool_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let input = request.get("input").cloned().unwrap_or_else(|| json!({}));
        let reason = request
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string);

        if tool_name == "AskUserQuestion" {
            let questions_raw = input.get("questions").cloned().unwrap_or_else(|| json!([]));
            let questions = parse_ask_user_questions(&input);
            self.pending_user_input
                .insert(request_id.clone(), questions_raw);
            return vec![AgentEvent::UserInputRequested {
                request_id,
                questions,
                delivery: UserInputDelivery::Blocking,
            }];
        }

        let request_type = classify_claude_tool(&tool_name);

        let detail = approval_detail(&tool_name, &input);
        let kind = match request_type {
            ClaudeRequestType::FileRead => ApprovalKind::FileRead { detail },
            ClaudeRequestType::ExecCommand => ApprovalKind::ExecCommand {
                command: input
                    .get("command")
                    .or_else(|| input.get("cmd"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                cwd: input.get("cwd").and_then(Value::as_str).map(str::to_string),
                reason,
            },
            ClaudeRequestType::FileChange => ApprovalKind::FileChange {
                changes: file_changes(&tool_name, &input),
                reason,
            },
            ClaudeRequestType::ToolUse => ApprovalKind::ToolUse {
                name: tool_name.clone(),
                input: input.clone(),
                detail,
            },
        };

        let mut options = vec![
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
        ];
        let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
        for suggestion in request
            .get("permission_suggestions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let scope = suggestion
                .get("destination")
                .and_then(Value::as_str)
                .unwrap_or("session");
            if let Some((_, suggestions)) = groups.iter_mut().find(|(known, _)| known == scope) {
                suggestions.push(suggestion.clone());
            } else {
                groups.push((scope.to_owned(), vec![suggestion.clone()]));
            }
        }
        let mut suggestions = HashMap::new();
        for (index, (scope, group)) in groups.into_iter().enumerate() {
            let id = format!("allow_suggestion:{index}");
            let label = match scope.as_str() {
                "session" => "Allow and remember for this session".to_owned(),
                "userSettings" => "Allow and save to user settings".to_owned(),
                "projectSettings" => "Allow and save to project settings".to_owned(),
                "localSettings" => "Allow and save to local settings".to_owned(),
                _ => format!("Allow and remember in {scope}"),
            };
            options.push(ApprovalOption {
                id: id.clone(),
                label,
                kind: ApprovalOptionKind::AllowAlways,
            });
            suggestions.insert(id, json!(group));
        }
        self.pending_approvals
            .insert(request_id.clone(), PendingApproval { input, suggestions });
        let parent_item_id = request
            .get("agent_id")
            .and_then(Value::as_str)
            .and_then(|agent_id| self.task_tools.get(agent_id))
            .cloned();
        vec![AgentEvent::ApprovalRequested(ApprovalRequest {native_execution: Some(json!({"tool_name":tool_name,"action":tool_name,"arguments":request["input"],"resource_kind":if tool_name=="Bash"{"process"}else if matches!(tool_name.as_str(),"Write"|"Edit"|"MultiEdit"){"file"}else{"mcp"}})), tool_call_id: request["tool_use_id"].as_str().map(str::to_owned),
            id: request_id,
            turn_id: self.current_turn_id.clone(),
            parent_item_id,
            kind,
            options,
        })]
    }

    fn on_result(&mut self, msg: &Value) -> Vec<AgentEvent> {
        if let Some(id) = msg.get("uuid").and_then(Value::as_str)
            && !self.result_ids.insert(id.to_owned())
        {
            return Vec::new();
        }
        self.awaiting_turn_checkpoint = false;
        let mut events = self.observe_stop_reason(msg.get("stop_reason").and_then(Value::as_str));
        let turn_id = self
            .current_turn_id
            .take()
            .unwrap_or_else(|| format!("turn-{}", self.turn_counter.max(1)));
        self.last_completed_turn=Some(turn_id.clone());
        // No message outlives its turn, so the block counters can go.
        self.assistant_messages.clear();
        let mut status = result_status(msg, self.stop_reason.as_deref());
        if std::mem::take(&mut self.interrupt_pending) && status != TurnStatus::Completed {
            status = TurnStatus::Interrupted;
        }
        let usage = msg.get("usage").map(|u| {
            let aggregate = map_usage(u, msg.get("modelUsage"));
            let processed = [
                "input_tokens",
                "cache_read_input_tokens",
                "cache_creation_input_tokens",
                "output_tokens",
            ]
            .into_iter()
            .any(|key| u.get(key).and_then(Value::as_u64).is_some())
            .then(|| crate::processed_tokens(aggregate));
            let mut usage = self.latest_usage;
            // Select capacity for the main model; a child's larger window is irrelevant.
            usage.context_window = self
                .last_served_model
                .as_deref()
                .and_then(|model| {
                    msg.get("modelUsage")?
                        .as_object()?
                        .iter()
                        .find(|(key, value)| {
                            key.split('[').next() == Some(model)
                                || value.get("canonicalModel").and_then(Value::as_str)
                                    == Some(model)
                        })?
                        .1
                        .get("contextWindow")?
                        .as_u64()
                })
                .or_else(|| {
                    msg.get("modelUsage")?
                        .as_object()
                        .filter(|m| m.len() == 1)?
                        .values()
                        .next()?
                        .get("contextWindow")?
                        .as_u64()
                })
                .or(usage.context_window);
            usage.turn_processed_tokens = processed;
            // The timeline owns lifetime accumulation, including restored history.
            usage.total_processed_tokens = None;
            usage.cost_usd = msg.get("total_cost_usd").and_then(Value::as_f64);
            usage.duration_ms = msg.get("duration_ms").and_then(Value::as_u64);
            self.latest_usage = usage;
            usage
        });
        if status == TurnStatus::Failed {
            // The `result` field carries the CLI's own error text (API errors,
            // crashes); a bare "failed" turn marker would discard it.
            let detail = msg
                .get("result")
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| msg.to_string());
            let subtype = msg
                .get("subtype")
                .and_then(Value::as_str)
                .unwrap_or("error");
            events.push(AgentEvent::Error {
                message: format!("claude turn failed ({subtype}): {detail}"),
                fatal: false,
            });
            if let Some(resets_at) = self.limit_resets_at.take() {
                events.push(AgentEvent::UsageLimitReached { resets_at });
            }
        }
        self.limit_resets_at = None;
        if self.stop_reason.as_deref() == Some("refusal") {
            let detail = msg
                .get("result")
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())
                .unwrap_or("Claude blocked this turn after a safety classifier refusal.")
                .to_owned();
            events.push(AgentEvent::TurnBlocked {
                category: self.pending_refusal_category.take(),
                model: self.expected_model.clone(),
                detail,
            });
        }
        // Publish the current background-task liveness with every result, not
        // only task-notification-origin ones. `background_tasks_changed []`
        // deliberately withholds the zero (see `on_background_tasks_changed`),
        // so a result is the only place the runtime can learn the tasks are
        // gone — and gating that on the CLI's `origin.kind` shape left the
        // count pinned above zero (session stuck "Working") whenever the
        // follow-up result arrived with a different origin. Re-publishing an
        // unchanged count is harmless; a parked runtime still observes zero
        // only after all completion output has landed.
        events.push(AgentEvent::BackgroundTasksChanged {
            count: self.background_tasks.len(),
        });
        if let Some(raw)=msg.get("usage") {
            let aggregate=map_usage(raw,None);
            events.push(AgentEvent::UsageDetail{scope:"turn_snapshot".into(),usage_id:turn_id.clone(),usage:json!({"input_tokens":aggregate.input_tokens,"output_tokens":aggregate.output_tokens,"cache_read_tokens":aggregate.cached_input_tokens,"cache_write_tokens":aggregate.cache_write_tokens,"input_includes_cache":false})});
        }
        if let Some(cost)=msg.get("total_cost_usd").and_then(Value::as_f64).filter(|cost|cost.is_finite()&&*cost>=0.){events.push(AgentEvent::UsageDetail{scope:"session_cumulative".into(),usage_id:"session_cost".into(),usage:json!({"cost_usd":cost,"cost_kind":"estimated"})});}
        events.push(AgentEvent::TurnCompleted {
            turn_id,
            status,
            usage,
        });
        events
    }
}

/// Claude Code keeps the turn open while it retries a failed API request, up to
/// `max_retries` times, so each attempt is a non-fatal error rather than an end.
fn api_retry_notice(msg: &Value) -> AgentEvent {
    let count = |key| msg.get(key).and_then(Value::as_u64);
    let mut cause = msg
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    if let Some(status) = count("error_status") {
        cause.push_str(&format!(", HTTP {status}"));
    }
    if let Some(waited) = msg
        .pointer("/no_response/waited_ms")
        .and_then(Value::as_u64)
    {
        cause.push_str(&format!(", no response in {}s", waited / 1000));
    }
    let attempt = count("attempt").map_or("?".into(), |n| n.to_string());
    let max = count("max_retries").map_or("?".into(), |n| n.to_string());
    let delay =
        count("retry_delay_ms").map_or("?".into(), |ms| format!("{:.1}s", ms as f64 / 1000.0));
    AgentEvent::Error {
        message: format!("claude API request failed ({cause}); retry {attempt}/{max} in {delay}"),
        fatal: false,
    }
}

fn is_file_tool(name: &str) -> bool {
    matches!(name, "Write" | "Edit" | "MultiEdit" | "NotebookEdit")
}

fn subagent_status(status: &str) -> ItemStatus {
    match status {
        "completed" | "done" | "succeeded" => ItemStatus::Completed,
        "failed" | "error" | "cancelled" | "canceled" | "stopped" | "killed" | "interrupted" => {
            ItemStatus::Failed
        }
        _ => ItemStatus::InProgress,
    }
}

pub(crate) fn one_line_summary(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Approval categories; tools without a dedicated category use `ToolUse`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeRequestType {
    FileRead,
    ExecCommand,
    FileChange,
    ToolUse,
}

/// The tools that spawn a subagent: `Agent` and its former name `Task`.
/// Coordination tools such as `ListAgents` and `SendMessage` do not.
pub(crate) fn is_agent_tool(normalized: &str) -> bool {
    matches!(normalized, "agent" | "task")
}

/// The in-progress Subagent snapshot for an Agent tool call. The call only
/// carries a model alias when the caller picked one; the resolved model and
/// effort arrive with the child's first assistant message.
pub(crate) fn spawned_subagent(input: &Value) -> ItemContent {
    ItemContent::Subagent {
        agent_type: input
            .get("subagent_type")
            .and_then(Value::as_str)
            .unwrap_or("subagent")
            .to_owned(),
        description: subagent_description(input),
        status: ItemStatus::InProgress,
        summary: None,
        model: input
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty())
            .map(str::to_owned),
        effort: None,
    }
}

fn subagent_description(input: &Value) -> String {
    input
        .get("description")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            input
                .get("prompt")
                .and_then(Value::as_str)
                .map(|prompt| prompt.chars().take(200).collect())
                .unwrap_or_default()
        })
}

/// Classify approvals with an ordered substring matcher. Read-only matches
/// take precedence (`WebSearch` → `FileRead`); agent tools use `ToolUse` even
/// when their names also match command or file-change substrings.
fn classify_claude_tool(name: &str) -> ClaudeRequestType {
    let n = name.to_lowercase();
    if n == "read"
        || n.contains("read file")
        || n.contains("view")
        || n.contains("grep")
        || n.contains("glob")
        || n.contains("search")
    {
        return ClaudeRequestType::FileRead;
    }
    if is_agent_tool(&n) {
        return ClaudeRequestType::ToolUse;
    }
    if n.contains("bash") || n.contains("command") || n.contains("shell") || n.contains("terminal")
    {
        return ClaudeRequestType::ExecCommand;
    }
    if n.contains("edit")
        || n.contains("write")
        || n.contains("file")
        || n.contains("patch")
        || n.contains("replace")
        || n.contains("create")
        || n.contains("delete")
    {
        return ClaudeRequestType::FileChange;
    }
    ClaudeRequestType::ToolUse
}

/// Summarize a command, subagent brief, or tool input for its approval prompt.
fn approval_detail(tool_name: &str, input: &Value) -> String {
    if let Some(cmd) = input
        .get("command")
        .or_else(|| input.get("cmd"))
        .and_then(Value::as_str)
    {
        let clipped: String = cmd.trim().chars().take(400).collect();
        return format!("{tool_name}: {clipped}");
    }
    if is_agent_tool(&tool_name.to_lowercase()) {
        let body = subagent_description(input);
        return match input
            .get("subagent_type")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            Some(subtype) => format!("{subtype}: {body}"),
            None => body,
        };
    }
    let json = serde_json::to_string(input).unwrap_or_default();
    if json.chars().count() <= 400 {
        format!("{tool_name}: {json}")
    } else {
        let clipped: String = json.chars().take(397).collect();
        format!("{tool_name}: {clipped}...")
    }
}

/// Parse `AskUserQuestion` tool input into canonical [`UserInputQuestion`]s
/// with the complete question text as `id` (falling back to `q-<index>`);
/// options and empty labels are preserved (the Claude side does not filter).
fn parse_ask_user_questions(input: &Value) -> Vec<UserInputQuestion> {
    let questions = match input.get("questions").and_then(Value::as_array) {
        Some(q) => q,
        None => return Vec::new(),
    };
    questions
        .iter()
        .enumerate()
        .map(|(index, q)| {
            let question_text = q.get("question").and_then(Value::as_str);
            let id = match question_text {
                Some(t) if !t.is_empty() => t.to_owned(),
                _ => format!("q-{index}"),
            };
            let header = q
                .get("header")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("Question {}", index + 1));
            let question = question_text.unwrap_or("").to_owned();
            let options = q
                .get("options")
                .and_then(Value::as_array)
                .map(|opts| {
                    opts.iter()
                        .map(|opt| UserInputOption {
                            label: opt
                                .get("label")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned(),
                            description: opt
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let multi_select = q
                .get("multiSelect")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            UserInputQuestion { contract: crate::native_question_contract(q), 
                id,
                header,
                question,
                options,
                multi_select,
                prefill: None,
            }
        })
        .collect()
}

/// Derive canonical [`FileChange`]s from a file-editing tool's input.
fn file_changes(name: &str, input: &Value) -> Vec<FileChange> {
    let path = input
        .get("file_path")
        .or_else(|| input.get("notebook_path"))
        .or_else(|| input.get("path"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    match name {
        "Write" => {
            let content = input.get("content").and_then(Value::as_str).unwrap_or("");
            vec![FileChange {
                path,
                kind: FileChangeKind::Create,
                diff: (!content.is_empty()).then(|| {
                    content
                        .lines()
                        .map(|l| format!("+{l}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                }),
            }]
        }
        "Edit" => {
            let old = input
                .get("old_string")
                .and_then(Value::as_str)
                .unwrap_or("");
            let new = input
                .get("new_string")
                .and_then(Value::as_str)
                .unwrap_or("");
            let mut diff = String::new();
            for l in old.lines() {
                diff.push('-');
                diff.push_str(l);
                diff.push('\n');
            }
            for l in new.lines() {
                diff.push('+');
                diff.push_str(l);
                diff.push('\n');
            }
            vec![FileChange {
                path,
                kind: FileChangeKind::Modify,
                diff: (!diff.is_empty()).then(|| diff.trim_end().to_string()),
            }]
        }
        _ => vec![FileChange {
            path,
            kind: FileChangeKind::Modify,
            diff: None,
        }],
    }
}

/// Render Claude's structured edit hunks as a unified diff body.
fn render_structured_patch(value: &Value) -> Option<String> {
    let hunks = value.as_array()?;
    let mut rendered = Vec::new();
    for hunk in hunks {
        let old_start = hunk.get("oldStart").and_then(Value::as_u64)?;
        let old_lines = hunk.get("oldLines").and_then(Value::as_u64)?;
        let new_start = hunk.get("newStart").and_then(Value::as_u64)?;
        let new_lines = hunk.get("newLines").and_then(Value::as_u64)?;
        rendered.push(format!(
            "@@ -{old_start},{old_lines} +{new_start},{new_lines} @@"
        ));
        rendered.extend(
            hunk.get("lines")
                .and_then(Value::as_array)?
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned),
        );
    }
    (!rendered.is_empty()).then(|| rendered.join("\n"))
}

/// Flatten a `tool_result` content field (string or block array) into text.
fn tool_result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => {
            let mut parts = Vec::new();
            for block in blocks {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    parts.push(text.to_string());
                } else {
                    parts.push(block.to_string());
                }
            }
            parts.join("\n")
        }
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn result_status(msg: &Value, stop_reason: Option<&str>) -> TurnStatus {
    let subtype = msg.get("subtype").and_then(Value::as_str);
    let is_error = msg.get("is_error").and_then(Value::as_bool);
    if subtype == Some("success") && is_error != Some(true) {
        return TurnStatus::Completed;
    }
    if matches!(
        subtype,
        Some("interrupted" | "interrupt" | "cancelled" | "canceled" | "aborted")
    ) || matches!(
        stop_reason,
        Some("interrupted" | "interrupt" | "cancelled" | "canceled" | "aborted")
    ) {
        return TurnStatus::Interrupted;
    }
    // Older CLI error results have no structured interrupt marker. Restrict the
    // legacy prose heuristic to those non-success results so ordinary answers
    // mentioning cancellation cannot be misclassified.
    let haystack = format!(
        "{} {}",
        msg.get("result")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        msg.get("subtype")
            .and_then(Value::as_str)
            .unwrap_or_default()
    )
    .to_lowercase();
    if haystack.contains("interrupt") || haystack.contains("abort") || haystack.contains("cancel") {
        TurnStatus::Interrupted
    } else {
        TurnStatus::Failed
    }
}

fn map_usage(usage: &Value, model_usage: Option<&Value>) -> TokenUsage {
    let get = |k: &str| usage.get(k).and_then(Value::as_u64);
    let input = get("input_tokens");
    let cache_read = get("cache_read_input_tokens");
    let cache_creation = get("cache_creation_input_tokens");
    let output = get("output_tokens");

    let used = [input, cache_read, cache_creation, output]
        .into_iter()
        .flatten()
        .sum::<u64>();
    let used_tokens = (used > 0).then_some(used);

    let context_window = model_usage.and_then(Value::as_object).and_then(|m| {
        m.values()
            .filter_map(|v| v.get("contextWindow").and_then(Value::as_u64))
            .max()
    });

    TokenUsage {
        context_window,
        input_tokens: input,
        cached_input_tokens: cache_read,
        cache_write_tokens: cache_creation,
        reasoning_tokens: get("reasoning_tokens"),
        output_tokens: output,
        used_tokens,
        // Session-cumulative total is stamped by the caller (`on_result`); the
        // streaming/partial usage path leaves it unset.
        ..TokenUsage::default()
    }
}

/// Parse Claude system-init `slash_commands` (→ [`ProviderCommandKind::Command`])
/// and `skills` (→ [`ProviderCommandKind::Skill`]) into [`ProviderCommand`]s.
/// Both are arrays of bare name strings; the CLI supplies no descriptions. A
/// skill is listed under both, which is how the runtime knows a `$name`
/// mention of it is sent as the `/name` command the CLI expands.
fn parse_provider_commands(init: &Value) -> Vec<ProviderCommand> {
    let mut out = Vec::new();
    let mut push = |field: &str, kind: ProviderCommandKind| {
        if let Some(names) = init.get(field).and_then(Value::as_array) {
            for name in names.iter().filter_map(Value::as_str) {
                let name = name.trim();
                if !name.is_empty() {
                    out.push(ProviderCommand {
                        name: name.to_owned(),
                        description: None,
                        kind,
                    });
                }
            }
        }
    };
    push("slash_commands", ProviderCommandKind::Command);
    push("skills", ProviderCommandKind::Skill);
    out
}

fn is_todo_tool(name: &str) -> bool {
    name.to_lowercase().contains("todowrite")
}

/// Map `TodoWrite` input `{ todos: [{ content, status, activeForm? }] }` to
/// plan steps (content → step, fallback `"Task"`; completed/in_progress →
/// Completed/InProgress, else Pending). `activeForm` is ignored.
fn extract_plan_steps_from_todo(input: &Value) -> Option<Vec<PlanStep>> {
    let todos = input.get("todos").and_then(Value::as_array)?;
    if todos.is_empty() {
        return None;
    }
    let steps = todos
        .iter()
        .filter(|todo| todo.is_object())
        .map(|todo| {
            let step = todo
                .get("content")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("Task")
                .to_owned();
            let status = match todo.get("status").and_then(Value::as_str) {
                Some("completed") => PlanStepStatus::Completed,
                Some("in_progress") => PlanStepStatus::InProgress,
                _ => PlanStepStatus::Pending,
            };
            PlanStep { step, status }
        })
        .collect();
    Some(steps)
}

fn has_boolean_option(spec: &ModelSpec, id: &str) -> bool {
    spec.options
        .iter()
        .any(|o| matches!(o, OptionDescriptor::Boolean { id: oid, .. } if oid == id))
}

/// Resolve the effort selection against the model's `reasoningEffort`
/// descriptor: an accepted listed value wins, else the descriptor default.
/// Returns `None` when the model has no reasoning selector (e.g. Haiku).
fn resolve_claude_effort(spec: Option<&ModelSpec>, raw: Option<&str>) -> Option<String> {
    let spec = spec?;
    let (options, default_value) = spec.options.iter().find_map(|o| match o {
        OptionDescriptor::Select {
            id,
            options,
            default_value,
            ..
        } if id == "reasoningEffort" => Some((options, default_value)),
        _ => None,
    })?;
    if let Some(raw) = raw
        && options.iter().any(|o| o.value == raw)
    {
        return Some(raw.to_owned());
    }
    default_value.clone()
}

/// Run `claude --version` and parse the semver triple; `None` on any failure.
async fn claude_version(binary: Option<&Path>, launch_env: &LaunchEnv) -> Option<(u32, u32, u32)> {
    // Resolve through the PATH search (PATHEXT-aware: on Windows the CLI only
    // exists as `claude.cmd`), falling back to the bare name so the failure is
    // reported by the OS exactly as before.
    let bin = crate::resolve_binary(binary, "claude")
        .unwrap_or_else(|_| std::path::PathBuf::from("claude"));
    crate::process::probe_version(&bin, launch_env, ProviderKind::ClaudeCode).await
}

/// List Claude's models: the manifest catalog (refreshed per `refresh`),
/// gated by the installed CLI version.
pub async fn list_models(
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
    refresh: CatalogRefresh,
) -> Result<Vec<ModelSpec>, AgentError> {
    crate::process::unblock(move || {
        crate::claude_manifest::refresh(refresh.cache_dir.as_deref(), refresh.network)
    })
    .await;
    let version = claude_version(binary_path.as_deref(), &launch_env).await;
    Ok(crate::claude_manifest::current().models_for_version(version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_permission_switches_publish_only_confirmed_values() {
        let mut mapper = Mapper::new();
        for mode in [
            "default",
            "acceptEdits",
            "auto",
            "dontAsk",
            "bypassPermissions",
        ] {
            let previous = mapper.applied_permission_mode.clone();
            let request = mapper.set_permission_mode_request_str(mode);
            assert_eq!(
                request["request"],
                json!({"subtype":"set_permission_mode", "mode":mode})
            );
            assert_eq!(mapper.applied_permission_mode, previous);
            let events = mapper.on_message(json!({"type":"control_response", "response": {
                "subtype":"success", "request_id":request["request_id"], "response":{}
            }}));
            assert!(
                matches!(events.as_slice(), [AgentEvent::ProviderOptions { selections, .. }]
                if selections.iter().any(|selection| selection.id == "permissionMode" && selection.value == mode))
            );
        }
        let request = mapper.set_permission_mode_request_str("auto");
        let events = mapper.on_message(json!({"type":"control_response", "response": {
            "subtype":"error", "request_id":request["request_id"], "error":"unsupported mode"
        }}));
        assert!(
            matches!(events.as_slice(), [AgentEvent::Warning { message }] if message.contains("unsupported mode"))
        );
        assert_eq!(mapper.applied_permission_mode, "bypassPermissions");

        let request = mapper.set_permission_mode_request_str("auto");
        let events = mapper.on_message(json!({"type":"control_response", "response": {
            "subtype":"success", "request_id":request["request_id"], "response":{"permissionMode":"dontAsk"}
        }}));
        assert!(
            matches!(events.as_slice(), [AgentEvent::ProviderOptions { selections, .. }] if selections[0].value == "dontAsk")
        );

        let request = mapper.set_permission_mode_request_str("auto");
        let reason = "Cannot set permission mode to auto: auto mode unavailable for this model";
        let events = mapper.on_message(json!({"type":"control_response", "response": {
            "subtype":"error", "request_id":request["request_id"],
            "error":reason, "error_code":"auto_mode_model"
        }}));
        assert!(
            events.iter().any(|event| matches!(event,
                AgentEvent::ProviderOptions { descriptors, selections }
                if selections[0].value == "dontAsk" && matches!(&descriptors[0],
                    OptionDescriptor::Select { options, .. }
                    if options.iter().any(|option| option.value == "auto"
                        && option.unavailable.as_deref() == Some(reason))))),
            "the provider's explicit auto unavailability must reach the picker: {events:?}"
        );

        let mut auto = Mapper::new_configured("auto".into(), false, None);
        let events = auto.on_message(json!({"type":"system", "subtype":"init", "session_id":"native", "permissionMode":"default"}));
        let descriptor = events
            .iter()
            .find_map(|event| match event {
                AgentEvent::ProviderOptions { descriptors, .. } => descriptors.first(),
                _ => None,
            })
            .unwrap();
        assert!(
            matches!(descriptor, OptionDescriptor::Select { options, .. }
            if options.iter().any(|option| option.value == "auto" && option.unavailable.as_deref() == Some("Not available: the session started in another mode")))
        );
        let events = auto.on_message(json!({"type":"system", "subtype":"init", "session_id":"native", "permissionMode":"plan"}));
        assert!(
            matches!(events.as_slice(), [AgentEvent::ProviderOptions { descriptors, selections }]
            if selections[0].value == "plan" && matches!(&descriptors[0], OptionDescriptor::Select { options, .. }
                if options.iter().any(|option| option.value == "plan" && option.unavailable.as_deref() == Some("Entered by the agent"))))
        );

        let mut supported = Mapper::new_configured("auto".into(), false, None);
        supported.on_message(json!({"type":"system", "subtype":"init", "session_id":"supported", "permissionMode":"auto"}));
        let events = supported.on_message(json!({"type":"system", "subtype":"init", "session_id":"supported", "permissionMode":"plan"}));
        assert!(
            matches!(events.as_slice(), [AgentEvent::ProviderOptions { descriptors, selections }]
            if selections[0].value == "plan" && matches!(&descriptors[0], OptionDescriptor::Select { options, .. }
                if options.iter().any(|option| option.value == "plan" && option.unavailable.as_deref() == Some("Entered by the agent"))
                && options.iter().any(|option| option.value == "auto" && option.unavailable.is_none()))),
            "native plan entry must preserve confirmed auto availability: {events:?}"
        );

        for confirmation in ["control_response", "init"] {
            let mut recovered = Mapper::new_configured("auto".into(), false, None);
            recovered.on_message(json!({"type":"system", "subtype":"init", "session_id":"recovered", "permissionMode":"default"}));
            let message = if confirmation == "init" {
                json!({"type":"system", "subtype":"init", "session_id":"recovered", "permissionMode":"auto"})
            } else {
                let request = recovered.set_permission_mode_request_str("auto");
                json!({"type":"control_response", "response": {
                    "subtype":"success", "request_id":request["request_id"], "response":{"permissionMode":"auto"}
                }})
            };
            let events = recovered.on_message(message);
            assert!(
                matches!(events.as_slice(), [AgentEvent::ProviderOptions { descriptors, selections }]
                if selections[0].value == "auto" && matches!(&descriptors[0], OptionDescriptor::Select { options, .. }
                    if options.iter().any(|option| option.value == "auto" && option.unavailable.is_none()))),
                "{confirmation} confirmation must restore auto availability: {events:?}"
            );
        }
    }

    #[test]
    fn native_permission_requests_apply_only_the_chosen_suggestion_group() {
        let input = json!({"file_path":"file.rs", "content":"changed"});
        let session =
            json!({"type":"addRules", "rules":[{"toolName":"Write"}], "destination":"session"});
        let user = json!({"type":"setMode", "mode":"acceptEdits", "destination":"userSettings"});
        for name in [
            "Write",
            "EnterPlanMode",
            "ExitPlanMode",
            "mcp__tcode_report__report_result",
        ] {
            for choice in [
                "allow",
                "deny",
                "allow_suggestion:0",
                "allow_suggestion:1",
                "cancel",
            ] {
                let mut mapper = Mapper::new();
                let events = mapper.on_message(
                    json!({"type":"control_request", "request_id":"native", "request": {
                        "subtype":"can_use_tool", "tool_name":name, "input":input,
                        "permission_suggestions":[session, user]
                    }}),
                );
                let [AgentEvent::ApprovalRequested(request)] = events.as_slice() else {
                    panic!("{events:?}")
                };
                assert_eq!(
                    request
                        .options
                        .iter()
                        .map(|option| option.id.as_str())
                        .collect::<Vec<_>>(),
                    ["allow", "deny", "allow_suggestion:0", "allow_suggestion:1"]
                );
                assert_eq!(
                    request.options[2].label,
                    "Allow and remember for this session"
                );
                assert!(mapper.take_outgoing().is_empty());
                let decision = if choice == "cancel" {
                    ApprovalDecision::Cancel
                } else {
                    ApprovalDecision::Option(choice.into())
                };
                let response = mapper.build_approval_response("native", decision).unwrap();
                let body = &response["response"]["response"];
                if matches!(choice, "deny" | "cancel") {
                    assert_eq!(body["behavior"], "deny");
                    assert_eq!(
                        body["message"],
                        if choice == "cancel" {
                            "User cancelled tool execution."
                        } else {
                            "User declined tool execution."
                        }
                    );
                } else {
                    assert_eq!(body["behavior"], "allow");
                    assert_eq!(body["updatedInput"], input);
                }
                match choice {
                    "allow_suggestion:0" => {
                        assert_eq!(body["updatedPermissions"], json!([session]))
                    }
                    "allow_suggestion:1" => assert_eq!(body["updatedPermissions"], json!([user])),
                    _ => assert!(body.get("updatedPermissions").is_none()),
                }
            }
        }
    }

    /// A stand-in CLI that records its arguments, one per line, in `directory/argv`.
    #[cfg(unix)]
    fn recording_claude(directory: &std::path::Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::create_dir_all(directory).unwrap();
        let binary = directory.join("claude");
        std::fs::write(&binary, "#!/bin/sh\nif [ \"$1\" = --version ]; then printf '2.1.200\\n'; exit 0; fi\nprintf '%s\\n' \"$@\" > argv\nprintf '%s\\n' '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"native\",\"permissionMode\":\"auto\"}'\nwhile IFS= read -r line; do :; done\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        binary
    }

    #[cfg(unix)]
    async fn shut_down(handle: SessionHandle) {
        handle
            .commands
            .send(SessionCommand::Shutdown)
            .await
            .unwrap();
        while let Ok(event) = handle.events.recv().await {
            if matches!(event, AgentEvent::SessionClosed { .. }) {
                break;
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn instructions_append_to_the_system_prompt_on_creation_and_resume() {
        smol::block_on(async {
            let directory = std::env::temp_dir().join(format!(
                "agent-claude-instructions-{}",
                uuid::Uuid::new_v4()
            ));
            let binary = recording_claude(&directory);
            let instructions =
                "<pull_request_linking>\nLink every PR.\n</pull_request_linking>\n\n";
            for resume in [None, Some(ResumeCursor(json!({"session_id": "native"})))] {
                let handle = start(SessionOptions {
                    cwd: directory.clone(),
                    model: None,
                    resume: resume.clone(),
                    fork: false,
                    binary_path: Some(binary.clone()),
                    option_selections: Vec::new(),
                    mcp_servers: Vec::new(),
                    launch_env: LaunchEnv::default(),
                    extra_args: Vec::new(),
                    acp: None,
                    instructions: Some(instructions.into()),
                })
                .await
                .unwrap();
                while !matches!(
                    handle.events.recv().await.unwrap(),
                    AgentEvent::ProviderOptions { .. }
                ) {}
                let argv = std::fs::read_to_string(directory.join("argv")).unwrap();
                assert_eq!(resume.is_some(), argv.contains("--resume\nnative\n"));
                assert_eq!(
                    argv.matches(&format!("--append-system-prompt\n{instructions}"))
                        .count(),
                    1,
                    "{argv}"
                );
                shut_down(handle).await;
            }
            std::fs::remove_dir_all(directory).unwrap();
        });
    }

    #[cfg(unix)]
    #[test]
    fn launch_uses_native_permission_selection_and_enables_live_bypass() {
        smol::block_on(async {
            let directory = std::env::temp_dir()
                .join(format!("agent-claude-permission-{}", uuid::Uuid::new_v4()));
            let binary = recording_claude(&directory);
            let handle = start(SessionOptions {
                cwd: directory.clone(),
                model: None,
                resume: None,
                fork: false,
                binary_path: Some(binary),
                option_selections: vec![OptionSelection {
                    id: "permissionMode".into(),
                    value: json!("auto"),
                }],
                mcp_servers: Vec::new(),
                launch_env: LaunchEnv::default(),
                extra_args: Vec::new(),
                acp: None,
                instructions: None,
            })
            .await
            .unwrap();
            loop {
                let event = handle.events.recv().await.unwrap();
                if let AgentEvent::ProviderOptions { selections, .. } = event {
                    assert!(
                        selections
                            .iter()
                            .any(|selection| selection.id == "permissionMode"
                                && selection.value == "auto")
                    );
                    break;
                }
            }
            let argv = std::fs::read_to_string(directory.join("argv")).unwrap();
            let args: Vec<_> = argv.lines().collect();
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["--permission-mode", "auto"])
            );
            assert!(args.contains(&"--allow-dangerously-skip-permissions"));
            shut_down(handle).await;
            std::fs::remove_dir_all(directory).unwrap();
        });
    }

    #[test]
    fn fork_flag_requires_both_request_and_resume_cursor() {
        let resume = Some(ResumeCursor(json!({"session_id": "session-source"})));
        assert_eq!(
            resume_args(&resume, true),
            ["--resume", "session-source", "--fork-session"]
        );
        assert_eq!(resume_args(&resume, false), ["--resume", "session-source"]);
        assert!(resume_args(&None, true).is_empty());
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
            if count == 0 {
                assert!(args.is_empty());
                continue;
            }
            assert_eq!(args.len(), 2);
            assert_eq!(args[0], "--mcp-config");
            let config: Value = serde_json::from_str(&args[1]).unwrap();
            let servers = config["mcpServers"].as_object().unwrap();
            assert_eq!(servers.len(), count);
            for (name, url, authorization) in [
                ("tcode_preview", "http://p", "Bearer p"),
                ("tcode_orchestrate", "http://o", "Bearer o"),
                ("tcode_computer_use", "http://c", "Bearer c"),
            ]
            .into_iter()
            .take(count)
            {
                assert_eq!(
                    servers[name],
                    json!({
                        "type": "http",
                        "url": url,
                        "headers": {"Authorization": authorization},
                    })
                );
            }
        }
    }

    fn feed(mapper: &mut Mapper, line: &str) -> Vec<AgentEvent> {
        let msg: Value = serde_json::from_str(line).expect("valid json fixture line");
        mapper.on_message(msg)
    }

    /// Every `result` publishes the current background-task count immediately
    /// before its `TurnCompleted` (the runtime's only reliable zero signal).
    /// Assert that contract and return the completion event.
    #[track_caller]
    fn turn_completed_after_count(events: &[AgentEvent], count: usize) -> &AgentEvent {
        let at = events
            .iter()
            .position(|event| matches!(event, AgentEvent::TurnCompleted { .. }))
            .unwrap_or_else(|| panic!("no TurnCompleted in {events:?}"));
        assert!(
            at > 0
                && matches!(
                    &events[at - 1],
                    AgentEvent::BackgroundTasksChanged { count: published } if *published == count
                ),
            "TurnCompleted must follow BackgroundTasksChanged {{ count: {count} }}, got {events:?}"
        );
        &events[at]
    }

    #[test]
    fn mid_stream_fallback_is_published_before_the_response_finishes() {
        // Claude Code 2.1.299 against a local SSE endpoint using the documented
        // fallback boundary; only stream, assistant and result messages retained.
        let fixture = include_str!("../tests/fixtures/claude/mid-stream-fallback.jsonl");
        for requested in [Some("claude-fable-5".into()), None] {
            let mut mapper = Mapper::new_configured("default".into(), false, requested);
            mapper.start_turn();
            let mut switched = false;
            let mut completed = false;
            for line in fixture.lines() {
                let message: Value = serde_json::from_str(line).unwrap();
                let boundary = message
                    .pointer("/message/content/0/type")
                    .and_then(Value::as_str)
                    == Some("fallback");
                let events = mapper.on_message(message);
                if boundary {
                    assert!(!completed);
                    let notices = events
                        .iter()
                        .filter(|event| {
                            matches!(
                                event,
                                AgentEvent::ModelFallbackDetected { .. }
                                    | AgentEvent::ServedModel { .. }
                            )
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    assert!(
                        !events
                            .iter()
                            .any(|event| matches!(event, AgentEvent::TurnCompleted { .. }))
                    );
                    assert_eq!(
                        notices,
                        [
                            AgentEvent::ModelFallbackDetected {
                                expected: "claude-fable-5".into(),
                                actual: "claude-opus-4-8".into(),
                                category: None,
                                checkpoint_id: None,
                                parent_tool_use_id: None,
                            },
                            AgentEvent::ServedModel {
                                model: "claude-opus-4-8".into(),
                                reason: None
                            },
                        ]
                    );
                    switched = true;
                } else if switched {
                    assert!(
                        !events.iter().any(|event| matches!(
                            event,
                            AgentEvent::ModelFallbackDetected { .. }
                                | AgentEvent::ServedModel { .. }
                        )),
                        "later blocks must not repeat or undo the switch: {events:?}"
                    );
                }
                if events
                    .iter()
                    .any(|event| matches!(event, AgentEvent::TurnCompleted { .. }))
                {
                    assert!(switched, "the switch must be visible before completion");
                    completed = true;
                }
            }
            assert!(switched && completed);
        }
    }

    #[test]
    fn model_refusal_fallback_publishes_served_model_before_completion() {
        let mut mapper = Mapper::new();
        mapper.start_turn();
        let events = feed(
            &mut mapper,
            r#"{"type":"system","subtype":"model_refusal_fallback","trigger":"refusal","direction":"retry","scope":"session","original_model":"claude-fable-5","fallback_model":"claude-opus-4-8","request_id":"req_123","api_refusal_category":"cyber","api_refusal_explanation":null,"refused_user_message_uuid":"cf7703ac-420b-4038-9f32-582077b27352","content":"Safeguards flagged this message. Switched models.","session_id":"session-1","uuid":"system-1"}"#,
        );

        assert_eq!(
            events,
            [
                AgentEvent::ModelFallbackDetected {
                    expected: "claude-fable-5".into(),
                    actual: "claude-opus-4-8".into(),
                    category: Some(ClassifierCategory::Cyber),
                    checkpoint_id: Some("cf7703ac-420b-4038-9f32-582077b27352".into()),
                    parent_tool_use_id: None,
                },
                AgentEvent::ServedModel {
                    model: "claude-opus-4-8".into(),
                    reason: None,
                }
            ]
        );
    }

    #[test]
    fn assistant_model_changes_are_detected_without_family_rules() {
        for (expected, served, changed) in [
            ("claude-fable-5-1", "claude-fable-5", true),
            ("claude-fable-5-1", "claude-opus-5", true),
            ("claude-fable-5-1", "claude-opus-4-8", true),
            ("claude-fable-5-1", "claude-sonnet-4-5", true),
            ("claude-fable-5-1", "claude-haiku-4-5", true),
            ("claude-opus-4-8", "claude-opus-5", true),
            ("custom-model", "another-model", true),
            ("claude-fable-5-1", "claude-fable-5-1", false),
            ("claude-opus-5[1m]", "claude-opus-5", false),
            ("claude-opus-5", "CLAUDE-OPUS-5[1M]", false),
            ("claude-opus-5[2m]", "claude-opus-5", false),
            ("custom-model[variant]", "custom-model", true),
            ("claude-fable-5-1", "<synthetic>", false),
        ] {
            let mut mapper = Mapper::new_configured("default".into(), false, Some(expected.into()));
            mapper.start_turn();
            let events = mapper.on_message(json!({
                "type": "assistant",
                "message": { "id": "msg-change", "model": served, "content": [] },
                "parent_tool_use_id": null,
            }));
            let changes: Vec<_> = events
                .iter()
                .filter(|event| matches!(event, AgentEvent::ModelFallbackDetected { .. }))
                .collect();
            assert_eq!(
                changes.len(),
                usize::from(changed),
                "{expected} -> {served}"
            );
            if changed {
                assert!(matches!(changes[0], AgentEvent::ModelFallbackDetected {
                    expected: requested, actual, category: None,
                    checkpoint_id: None, parent_tool_use_id: None,
                } if requested == expected && actual == served));
            }
        }
    }

    #[test]
    fn model_changes_are_detected_at_stream_start_once_per_turn() {
        let mut mapper =
            Mapper::new_configured("default".into(), false, Some("claude-fable-5-1".into()));
        for served in ["claude-opus-5", "claude-opus-4-8", "claude-fable-5"] {
            let stream = json!({
                "type": "stream_event",
                "event": { "type": "message_start", "message": {
                    "id": "msg-change", "model": served,
                } },
                "parent_tool_use_id": null,
            });
            for _ in 0..2 {
                mapper.start_turn();
                // A child using another model must not consume the parent's guard.
                let mut child = stream.clone();
                child["parent_tool_use_id"] = json!("child-tool");
                let child_events = mapper.on_message(child);
                assert!(
                    !child_events
                        .iter()
                        .any(|event| matches!(event, AgentEvent::ModelFallbackDetected { .. }))
                );
                let first = mapper.on_message(stream.clone());
                assert_eq!(
                    first
                        .iter()
                        .filter(|event| matches!(
                            event, AgentEvent::ModelFallbackDetected { expected, actual, .. }
                                if expected == "claude-fable-5-1" && actual == served
                        ))
                        .count(),
                    1
                );
                let repeated = mapper.on_message(stream.clone());
                let completed = mapper.on_message(json!({
                    "type": "assistant", "message": {
                        "id": "msg-change", "model": served, "content": [],
                    }, "parent_tool_use_id": null,
                }));
                assert!(
                    !repeated
                        .iter()
                        .chain(&completed)
                        .any(|event| matches!(event, AgentEvent::ModelFallbackDetected { .. }))
                );
            }
        }
    }

    #[test]
    fn explicit_model_changes_ignore_only_context_suffix_differences() {
        for (actual, changed) in [("claude-opus-5", false), ("claude-opus-4-8", true)] {
            let mut mapper = Mapper::new();
            let events = mapper.on_message(json!({
                "type": "system", "subtype": "model_refusal_fallback",
                "original_model": "claude-opus-5[1m]", "fallback_model": actual,
            }));
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, AgentEvent::ModelFallbackDetected { .. }))
                    .count(),
                usize::from(changed)
            );
        }
    }

    #[test]
    fn classifier_refusal_result_emits_turn_blocked() {
        let mut mapper =
            Mapper::new_configured("default".into(), false, Some("claude-fable-5".into()));
        mapper.start_turn();

        let assistant_events = feed(
            &mut mapper,
            r#"{"type":"assistant","message":{"id":"msg-refusal","model":"<synthetic>","stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber","explanation":"This request triggered cyber content restrictions."},"content":[{"type":"text","text":"API Error: safeguards flagged this message. Details: `[cyber]`"}]},"parent_tool_use_id":null}"#,
        );
        assert_eq!(
            mapper.pending_refusal_category,
            Some(ClassifierCategory::Cyber)
        );
        assert!(
            assistant_events
                .iter()
                .any(|event| matches!(event, AgentEvent::Warning { .. }))
        );

        let events = feed(
            &mut mapper,
            r#"{"type":"result","subtype":"success","is_error":true,"stop_reason":"refusal","result":"API Error: safeguards flagged this message. Details: `[cyber]`","terminal_reason":"api_error","modelUsage":{"claude-fable-5":{"inputTokens":1,"outputTokens":0}}}"#,
        );

        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::TurnBlocked {
                category: Some(ClassifierCategory::Cyber),
                model: Some(model),
                detail,
            } if model == "claude-fable-5" && detail.contains("[cyber]")
        )));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::Error { fatal: false, .. }))
        );
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::TurnCompleted {
                status: TurnStatus::Failed,
                ..
            }
        )));
    }

    #[test]
    fn replayed_user_message_exposes_native_turn_checkpoint() {
        let mut mapper = Mapper::new();
        mapper.native_rewind = true;
        let turn_id = mapper.start_turn();
        let events = feed(
            &mut mapper,
            r#"{"type":"user","uuid":"checkpoint-1","message":{"content":[{"type":"text","text":"hello"}]}}"#,
        );
        assert!(matches!(
            events.as_slice(),
            [AgentEvent::TurnCheckpoint {
                turn_id: actual_turn,
                checkpoint_id,
            }] if actual_turn == &turn_id && checkpoint_id == "checkpoint-1"
        ));

        // Tool-result user envelopes and later steering echoes cannot replace
        // the checkpoint associated with the opening prompt.
        assert!(feed(
            &mut mapper,
            r#"{"type":"user","uuid":"tool-result","message":{"content":[{"type":"tool_result","tool_use_id":"missing","content":"ok"}]}}"#,
        )
        .is_empty());
    }

    #[test]
    fn combined_native_rewind_sequences_files_then_conversation() {
        let mut mapper = Mapper::new();
        mapper.native_rewind = true;
        let files = mapper
            .begin_rewind("checkpoint-2".into(), RewindMode::FilesAndConversation)
            .unwrap();
        assert_eq!(files["request"]["subtype"], "rewind_files");
        let files_id = files["request_id"].as_str().unwrap();
        let events = feed(
            &mut mapper,
            &json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": files_id,
                    "response": { "canRewind": true }
                }
            })
            .to_string(),
        );
        assert!(events.is_empty());
        let outgoing = mapper.take_outgoing();
        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0]["request"]["subtype"], "rewind_conversation");
        assert_eq!(
            outgoing[0]["request"]["target_message_uuid"],
            "checkpoint-2"
        );

        let conversation_id = outgoing[0]["request_id"].as_str().unwrap();
        let events = feed(
            &mut mapper,
            &json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": conversation_id,
                    "response": {
                        "rewound": true,
                        "prefillText": "original prompt"
                    }
                }
            })
            .to_string(),
        );
        assert!(matches!(
            events.as_slice(),
            [AgentEvent::RewindCompleted {
                checkpoint_id,
                mode: RewindMode::FilesAndConversation,
                prefill: Some(prefill), ..
            }] if checkpoint_id == "checkpoint-2" && prefill == "original prompt"
        ));
    }

    fn select(id: &str, value: Value) -> OptionSelection {
        OptionSelection {
            id: id.into(),
            value,
        }
    }

    fn settings(launch: &ClaudeLaunchOptions) -> Value {
        launch
            .settings_json
            .as_deref()
            .map(|settings| serde_json::from_str(settings).unwrap())
            .unwrap_or(Value::Null)
    }

    #[test]
    fn effort_map_drives_the_effort_flag() {
        let catalog = crate::claude_manifest::test_catalog();
        let resolve = |model: &str, effort: &str| {
            ClaudeLaunchOptions::resolve_with(
                &catalog,
                Some(model),
                &[select("reasoningEffort", json!(effort))],
            )
        };
        // Mapped to another value: ultracode → xhigh, plus the ultracode setting.
        let launch = resolve("test-wide", "ultracode");
        assert_eq!(launch.effort.as_deref(), Some("xhigh"));
        assert_eq!(settings(&launch)["ultracode"], true);
        // Per-profile downgrade (max → high) and passthrough.
        assert_eq!(
            resolve("test-narrow", "max").effort.as_deref(),
            Some("high")
        );
        assert_eq!(resolve("test-wide", "max").effort.as_deref(), Some("max"));
        assert_eq!(
            resolve("test-fixed", "xhigh").effort.as_deref(),
            Some("max")
        );
        assert_eq!(
            resolve("test-wide", "bogus").effort.as_deref(),
            Some("medium")
        );
        assert_eq!(
            ClaudeLaunchOptions::resolve_with(&catalog, Some("test-wide"), &[])
                .effort
                .as_deref(),
            Some("medium")
        );
        assert_eq!(resolve("test-plain", "low").effort, None);
        // Unknown model: no descriptor to resolve against, so no flag.
        assert_eq!(resolve("test-unknown", "high").effort, None);
    }

    #[test]
    fn version_gating_follows_manifest_bounds() {
        let catalog = crate::claude_manifest::test_catalog();
        let ids = |version: Option<(u32, u32, u32)>| -> Vec<String> {
            catalog
                .models_for_version(version)
                .into_iter()
                .map(|model| model.id)
                .collect()
        };
        // Ungated models are always listed, even without a known version.
        assert_eq!(ids(None), ["test-narrow", "test-plain"]);
        // minVersion is inclusive; maxVersionExclusive is not.
        assert_eq!(
            ids(Some((2, 1, 256))),
            ["test-fixed", "test-narrow", "test-plain"]
        );
        assert_eq!(
            ids(Some((2, 1, 257))),
            ["test-wide", "test-fixed", "test-narrow", "test-plain"]
        );
        assert_eq!(ids(Some((2, 1, 110))), ["test-narrow", "test-plain"]);
        assert_eq!(
            ids(Some((3, 0, 0))),
            ["test-wide", "test-narrow", "test-plain"]
        );
    }

    #[test]
    fn parse_semver_from_version_output() {
        assert_eq!(
            crate::parse_semver("2.1.206 (Claude Code)"),
            Some((2, 1, 206))
        );
        assert_eq!(crate::parse_semver("2.1.169"), Some((2, 1, 169)));
        assert_eq!(crate::parse_semver("nonsense"), None);
    }

    #[test]
    fn context_window_launch_semantics() {
        let catalog = crate::claude_manifest::test_catalog();
        let resolve = |model: &str, value: Option<Value>| {
            let selections: Vec<_> = value
                .map(|value| select("contextWindow", value))
                .into_iter()
                .collect();
            ClaudeLaunchOptions::resolve_with(&catalog, Some(model), &selections)
        };
        let auto_compact =
            |launch: &ClaudeLaunchOptions| settings(launch)["autoCompactWindow"].clone();

        // Default window of a 1M-default profile: the bare slug already runs
        // at 1M, so no suffix is sent (and none is needed for `1m` either).
        for value in [None, Some(json!("1m"))] {
            let launch = resolve("test-wide", value);
            assert_eq!(launch.model_id.as_deref(), Some("test-wide"));
            assert!(launch.settings_json.is_none());
        }
        assert_eq!(catalog.resolved_context_window("test-wide", &[]), 1_000_000);
        // A suffixed id still resolves to the same model.
        assert_eq!(
            catalog.resolved_context_window("test-wide[1m]", &[]),
            1_000_000
        );

        // Below the largest window: bare slug plus early compaction.
        let launch = resolve("test-wide", Some(json!("200k")));
        assert_eq!(launch.model_id.as_deref(), Some("test-wide"));
        assert_eq!(auto_compact(&launch), json!(200_000));
        let launch = resolve("test-wide", Some(json!(500_000)));
        assert_eq!(launch.model_id.as_deref(), Some("test-wide"));
        assert_eq!(auto_compact(&launch), json!(500_000));

        // 200k-default profile: only the 1M expansion carries the suffix.
        for value in [Some(json!("200k")), None] {
            let launch = resolve("test-narrow", value);
            assert_eq!(launch.model_id.as_deref(), Some("test-narrow"));
            assert_eq!(auto_compact(&launch), json!(200_000));
        }
        let launch = resolve("test-narrow", Some(json!("1m")));
        assert_eq!(launch.model_id.as_deref(), Some("test-narrow[1m]"));
        assert!(launch.settings_json.is_none());
        let launch = resolve("test-narrow", Some(json!(500_000)));
        assert_eq!(launch.model_id.as_deref(), Some("test-narrow[1m]"));
        assert_eq!(auto_compact(&launch), json!(500_000));

        // Fixed window: no suffix, smaller values compact early.
        assert_eq!(
            catalog.resolved_context_window("test-fixed", &[]),
            1_000_000
        );
        let launch = resolve("test-fixed", Some(json!("1m")));
        assert_eq!(launch.model_id.as_deref(), Some("test-fixed"));
        assert!(launch.settings_json.is_none());
        let launch = resolve("test-fixed", Some(json!(500_000)));
        assert_eq!(launch.model_id.as_deref(), Some("test-fixed"));
        assert_eq!(auto_compact(&launch), json!(500_000));

        // An alias launches as the canonical slug with the same semantics.
        let launch = resolve("WIDE", Some(json!("200k")));
        assert_eq!(launch.model_id.as_deref(), Some("test-wide"));
        assert_eq!(auto_compact(&launch), json!(200_000));
        let launch = resolve("test-wide.0[1m]", None);
        assert_eq!(launch.model_id.as_deref(), Some("test-wide"));

        // No context data at all falls back to 200k and never emits settings.
        assert_eq!(catalog.resolved_context_window("test-plain", &[]), 200_000);
        assert_eq!(
            catalog.resolved_context_window("test-unknown", &[]),
            200_000
        );
        let launch = resolve("test-plain", Some(json!(500_000)));
        assert_eq!(launch.model_id.as_deref(), Some("test-plain"));
        assert!(launch.settings_json.is_none());
    }

    #[test]
    fn boolean_options_reach_settings_only_when_supported() {
        let catalog = crate::claude_manifest::test_catalog();
        let both = [
            select("fastMode", json!(true)),
            select("thinking", json!(true)),
        ];
        let launch = ClaudeLaunchOptions::resolve_with(&catalog, Some("test-fixed"), &both);
        let fixed = settings(&launch);
        assert_eq!(fixed["fastMode"], true);
        assert!(fixed.get("alwaysThinkingEnabled").is_none());

        let launch = ClaudeLaunchOptions::resolve_with(&catalog, Some("test-plain"), &both);
        let plain = settings(&launch);
        assert_eq!(plain["alwaysThinkingEnabled"], true);
        assert!(plain.get("fastMode").is_none());
    }

    #[test]
    fn todo_write_maps_to_plan_updated() {
        let mut m = Mapper::new();
        m.start_turn();
        let evs = feed(
            &mut m,
            r#"{"type":"assistant","message":{"id":"msg_t","content":[{"type":"tool_use","id":"toolu_todo","name":"TodoWrite","input":{"todos":[{"content":"Build board","status":"completed","activeForm":"Building board"},{"content":"","status":"in_progress"},{"content":"Ship","status":"todo"}]}}]}}"#,
        );
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            AgentEvent::PlanUpdated { turn_id, steps, .. } => {
                assert_eq!(turn_id.as_deref(), Some("turn-1"));
                assert_eq!(steps.len(), 3);
                assert_eq!(steps[0].step, "Build board");
                assert_eq!(steps[0].status, PlanStepStatus::Completed);
                assert_eq!(steps[1].step, "Task"); // empty content fallback
                assert_eq!(steps[1].status, PlanStepStatus::InProgress);
                assert_eq!(steps[2].status, PlanStepStatus::Pending);
            }
            other => panic!("expected PlanUpdated, got {other:?}"),
        }
    }

    #[test]
    fn extra_permission_mode_arg_updates_launch_tracker() {
        let extra_args = vec!["--permission-mode".into(), "plan".into()];
        assert_eq!(
            effective_permission_mode("default", &extra_args),
            "plan",
            "the last provider launch argument overrides the internal flag"
        );

        let extra_args = vec!["--permission-mode=bypassPermissions".into()];
        assert_eq!(
            effective_permission_mode("plan", &extra_args),
            "bypassPermissions"
        );
    }

    #[test]
    fn user_message_carries_image_content_blocks() {
        let attachments = vec![
            Attachment {
                media_type: "image/png".into(),
                data_base64: "AAAA".into(),
                source_path: None,
            },
            Attachment {
                media_type: "image/jpeg".into(),
                data_base64: "BBBB".into(),
                source_path: None,
            },
        ];
        let msg = user_message("what color is this?", &attachments);
        let content = msg["message"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "what color is this?");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/png");
        assert_eq!(content[1]["source"]["data"], "AAAA");
        assert_eq!(content[2]["source"]["media_type"], "image/jpeg");
        // Text-only stays a single text block.
        let plain = user_message("hi", &[]);
        assert_eq!(plain["message"]["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn init_parses_slash_commands_and_skills() {
        let mut m = Mapper::new();
        let evs = feed(
            &mut m,
            r#"{"type":"system","subtype":"init","session_id":"s1","model":"claude-opus-4-8","slash_commands":["plan","review",""],"skills":["dataviz"]}"#,
        );
        assert!(
            matches!(evs.iter().find(|event| matches!(event, AgentEvent::SessionStarted { .. })),
            Some(AgentEvent::SessionStarted { provider_session_id, resume, model })
            if provider_session_id == "s1" && resume.0["session_id"] == "s1" && model.as_deref() == Some("claude-opus-4-8"))
        );
        let commands = evs
            .iter()
            .find_map(|event| match event {
                AgentEvent::ProviderCommands { commands } => Some(commands),
                _ => None,
            })
            .expect("init must publish native commands and skills");
        assert_eq!(
            commands
                .iter()
                .map(|command| (command.name.as_str(), command.kind))
                .collect::<Vec<_>>(),
            vec![
                ("plan", ProviderCommandKind::Command),
                ("review", ProviderCommandKind::Command),
                ("dataviz", ProviderCommandKind::Skill),
            ]
        );
        let repeated = feed(
            &mut m,
            r#"{"type":"system","subtype":"init","session_id":"s1","model":"claude-opus-4-8"}"#,
        );
        assert!(!repeated.iter().any(|event| matches!(
            event,
            AgentEvent::SessionStarted { .. } | AgentEvent::ProviderCommands { .. }
        )));
    }

    #[test]
    fn usage_scope_fixture_deduplicates_results_and_excludes_child_context() {
        let mut m = Mapper::new();
        let mut observations = Vec::new();
        let mut completions = 0;
        for line in include_str!("../tests/fixtures/claude/usage_scope.jsonl").lines() {
            for event in feed(&mut m, line) {
                match event {
                    AgentEvent::TokenUsage(u) => observations.push(u.used_tokens),
                    AgentEvent::TurnCompleted { usage: Some(u), .. } => {
                        completions += 1;
                        assert_eq!(u.used_tokens, Some(500260));
                        assert_eq!(u.turn_processed_tokens, Some(4001300));
                    }
                    _ => {}
                }
            }
        }
        assert_eq!(completions, 1);
        assert_eq!(
            observations,
            vec![
                Some(400150),
                Some(400150),
                Some(400150),
                Some(500260),
                Some(500260)
            ]
        );
    }

    #[test]
    fn recorded_compaction_retains_metadata_without_claiming_post_context() {
        let mut m = Mapper::new();
        let mut events = Vec::new();
        for line in include_str!("../tests/fixtures/claude/compaction_recorded.jsonl").lines() {
            events.extend(feed(&mut m, line));
        }
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], AgentEvent::ContextCompacted(c) if c.in_progress));
        assert!(matches!(&events[1], AgentEvent::ContextCompacted(c)
                if !c.in_progress && c.trigger.as_deref() == Some("manual")
                    && c.pre_tokens == Some(19555) && c.post_tokens == Some(2948)
                    && c.dropped_tokens == Some(16607) && c.duration_ms == Some(24455)));
        assert_eq!(m.latest_usage.used_tokens, None);
        assert_eq!(
            m.latest_usage.freshness,
            crate::ContextFreshness::AwaitingObservation
        );
    }

    #[test]
    fn recorded_multi_request_usage_keeps_latest_context() {
        let mut m = Mapper::new();
        m.start_turn();
        let mut observations = Vec::new();
        let mut completion = None;
        for line in include_str!("../tests/fixtures/claude/usage_recorded.jsonl").lines() {
            for event in feed(&mut m, line) {
                match event {
                    AgentEvent::TokenUsage(u) => observations.push(u.used_tokens),
                    AgentEvent::TurnCompleted { usage, .. } => completion = usage,
                    _ => {}
                }
            }
        }
        assert_eq!(
            observations,
            vec![
                Some(20250),
                Some(20250),
                Some(20250),
                Some(20250),
                Some(20635),
                Some(20635),
                Some(20635),
                Some(20764),
                Some(20764),
                Some(20764)
            ]
        );
        let usage = completion.unwrap();
        assert_eq!(usage.used_tokens, Some(20764));
        assert_eq!(usage.output_tokens, Some(5));
        assert_eq!(usage.turn_processed_tokens, Some(62084));
        assert_eq!(usage.context_window, Some(1000000));
    }

    #[test]
    fn streaming_usage_keeps_request_scope_at_completion() {
        let mut m = Mapper::new();
        m.start_turn();
        let start = feed(
            &mut m,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"usage-1","usage":{"input_tokens":100,"cache_read_input_tokens":400,"cache_creation_input_tokens":50,"output_tokens":0}}}}"#,
        );
        assert!(
            matches!(start.last(), Some(AgentEvent::TokenUsage(u)) if u.used_tokens == Some(550)),
            "message start must expose the measured request context: {start:?}"
        );
        let delta = feed(
            &mut m,
            r#"{"type":"stream_event","event":{"type":"message_delta","usage":{"output_tokens":20}}}"#,
        );
        assert!(
            matches!(delta.last(), Some(AgentEvent::TokenUsage(u)) if u.used_tokens == Some(550)),
            "output-only delta must retain input and cache: {delta:?}"
        );
        let result = feed(
            &mut m,
            r#"{"type":"result","subtype":"success","usage":{"input_tokens":1000,"cache_read_input_tokens":4000000,"output_tokens":200},"modelUsage":{"claude":{"contextWindow":1000000}}}"#,
        );
        assert!(result.iter().any(|e| matches!(e, AgentEvent::TurnCompleted { usage: Some(u), .. } if u.used_tokens == Some(550) && u.turn_processed_tokens == Some(4001200))), "result accounting must not replace occupancy: {result:?}");
        m.start_turn();
        let missing_start = feed(
            &mut m,
            r#"{"type":"stream_event","event":{"type":"message_delta","usage":{"output_tokens":1}}}"#,
        );
        assert!(
            missing_start.is_empty(),
            "a new turn cannot reuse the previous request identity: {missing_start:?}"
        );
        assert_eq!(m.latest_usage.freshness, crate::ContextFreshness::LastKnown);
        let unknown = feed(
            &mut m,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"usage-2"}}}"#,
        );
        assert!(
            matches!(unknown.last(), Some(AgentEvent::TokenUsage(u)) if u.used_tokens.is_none())
        );
        let output_only = feed(
            &mut m,
            r#"{"type":"stream_event","event":{"type":"message_delta","usage":{"output_tokens":1}}}"#,
        );
        assert!(
            matches!(output_only.last(), Some(AgentEvent::TokenUsage(u)) if u.used_tokens.is_none())
        );
    }

    #[test]
    fn result_reports_each_turn_traffic_for_timeline_accumulation() {
        let mut m = Mapper::new();
        m.start_turn();
        let evs = feed(
            &mut m,
            r#"{"type":"result","subtype":"success","usage":{"input_tokens":100,"output_tokens":20}}"#,
        );
        let first = match turn_completed_after_count(&evs, 0) {
            AgentEvent::TurnCompleted { usage, .. } => usage.unwrap(),
            other => panic!("expected TurnCompleted, got {other:?}"),
        };
        assert_eq!(first.turn_processed_tokens, Some(120));
        assert_eq!(first.total_processed_tokens, None);
        // Each result is a turn delta, independent of the mapper process lifetime.
        m.start_turn();
        let evs = feed(
            &mut m,
            r#"{"type":"result","subtype":"success","usage":{"input_tokens":30,"output_tokens":5}}"#,
        );
        let second = match turn_completed_after_count(&evs, 0) {
            AgentEvent::TurnCompleted { usage, .. } => usage.unwrap(),
            other => panic!("expected TurnCompleted, got {other:?}"),
        };
        assert_eq!(second.turn_processed_tokens, Some(35));
        assert_eq!(second.total_processed_tokens, None);
    }

    #[test]
    fn rejected_rate_limit_is_emitted_after_failed_result_error() {
        let mut m = Mapper::new();
        feed(
            &mut m,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1800000000,"rateLimitType":"five_hour","utilization":1.0}}"#,
        );

        let events = feed(
            &mut m,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"done","session_id":"sess-1"}"#,
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::UsageLimitReached { .. }))
        );

        let mut m = Mapper::new();
        assert!(feed(
            &mut m,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1800000000,"rateLimitType":"five_hour","utilization":1.0}}"#,
        )
        .is_empty());

        let events = feed(
            &mut m,
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"You've hit your limit","session_id":"sess-1"}"#,
        );
        let error_index = events
            .iter()
            .position(|event| matches!(event, AgentEvent::Error { .. }))
            .expect("failed result error");
        assert!(matches!(
            events.get(error_index + 1),
            Some(AgentEvent::UsageLimitReached {
                resets_at: 1_800_000_000
            })
        ));
    }

    #[test]
    fn streamed_deltas_preserve_block_identity_and_content_kind() {
        for (index, delta, expected_kind, expected_text) in [
            (
                0,
                json!({"type":"text_delta","text":"Hi"}),
                DeltaKind::AssistantText,
                "Hi",
            ),
            (
                1,
                json!({"type":"thinking_delta","thinking":"hmm"}),
                DeltaKind::ReasoningText,
                "hmm",
            ),
        ] {
            let mut m = Mapper::new();
            feed(
                &mut m,
                r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_1"}}}"#,
            );
            let events = feed(
                &mut m,
                &json!({"type":"stream_event", "event": {
                    "type":"content_block_delta", "index":index, "delta":delta
                }})
                .to_string(),
            );
            assert!(
                matches!(events.as_slice(), [AgentEvent::Delta { item_id, kind, text }]
                if item_id == &format!("msg_1:{index}") && *kind == expected_kind && text == expected_text)
            );
        }
    }

    /// Captured from Claude Code 2.1.294 with claude-opus-5-5: the thinking
    /// block opens with `content_block_start`, every `thinking_delta` is empty
    /// and the `assistant` line repeats the block with no text. The block's
    /// start is the one event that can announce the thinking, so it must
    /// become an in-progress reasoning item that the empty deltas and the
    /// empty completion leave alone.
    #[test]
    fn hidden_thinking_announces_itself_from_the_block_start() {
        let mut m = Mapper::new();
        feed(
            &mut m,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_h"}}}"#,
        );
        let started = feed(
            &mut m,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}}"#,
        );
        assert!(
            matches!(
                started.as_slice(),
                [AgentEvent::ItemStarted(ThreadItem {
                    id,
                    parent_item_id: None,
                    content: ItemContent::Reasoning { text },
                })] if id == "msg_h:0" && text.is_empty()
            ),
            "{started:?}"
        );
        for line in [
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}}"#,
            r#"{"type":"assistant","message":{"id":"msg_h","content":[{"type":"thinking","thinking":"","signature":"sig"}]}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}}"#,
        ] {
            let events = feed(&mut m, line);
            assert!(events.is_empty(), "{line}: {events:?}");
        }
    }

    /// Captured from a real session: the CLI splits one message across several
    /// `assistant` lines, each carrying a single-element `content` array — a
    /// (redacted, empty) thinking block first, then the text block. Enumerating
    /// each array on its own numbered the text block 0 while its deltas streamed
    /// under index 1, so the timeline rendered the paragraph twice: once live,
    /// once again from the completion. The completed item must land on the
    /// stream's id, and the empty thinking block must not complete an item.
    #[test]
    fn split_assistant_lines_keep_the_streams_block_numbering() {
        let mut m = Mapper::new();
        feed(
            &mut m,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_s"}}}"#,
        );
        let streamed = feed(
            &mut m,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Added percent_off."}}}"#,
        );
        let streamed_id = match &streamed[0] {
            AgentEvent::Delta { item_id, .. } => item_id.clone(),
            other => panic!("expected Delta, got {other:?}"),
        };
        assert_eq!(streamed_id, "msg_s:1");

        // First `assistant` line: the thinking block alone, with no content.
        let thinking = feed(
            &mut m,
            r#"{"type":"assistant","message":{"id":"msg_s","content":[{"type":"thinking","thinking":""}]}}"#,
        );
        assert!(
            thinking.is_empty(),
            "an empty thinking block must not blank the streamed reasoning: {thinking:?}"
        );

        // Second `assistant` line: the text block, which the stream called 1.
        let completed = feed(
            &mut m,
            r#"{"type":"assistant","message":{"id":"msg_s","content":[{"type":"text","text":"Added percent_off."}]}}"#,
        );
        match &completed[0] {
            AgentEvent::ItemCompleted(item) => {
                assert_eq!(
                    item.id, streamed_id,
                    "completion must reuse the streamed item id, or the text renders twice"
                );
            }
            other => panic!("expected ItemCompleted, got {other:?}"),
        }
    }

    #[test]
    fn image_read_results_exclude_failed_reads_and_image_generation() {
        for (name, failed, expected) in [
            ("Read", false, 1),
            ("Read", true, 0),
            ("generate_image", false, 0),
            ("mcp__browser__preview_screenshot", false, 1),
        ] {
            let mut mapper = Mapper::new();
            feed(&mut mapper, &json!({"type":"assistant","message":{"id":"m","content":[{"type":"tool_use","id":"read","name":name,"input":{"file_path":"/tmp/image.png"}}]}}).to_string());
            let events = feed(&mut mapper, &json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"read","is_error":failed,"content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AQID"}}]}]}}).to_string());
            let AgentEvent::ItemCompleted(ThreadItem {
                content:
                    ItemContent::ToolCall {
                        image_reads,
                        status,
                        ..
                    },
                ..
            }) = &events[0]
            else {
                panic!("tool completion: {events:?}")
            };
            assert_eq!(image_reads.len(), expected, "{name}, failed={failed}");
            assert_eq!(
                *status,
                if failed {
                    ItemStatus::Failed
                } else {
                    ItemStatus::Completed
                }
            );
            if expected != 0 {
                assert_eq!(image_reads[0].data_base64, "AQID");
            }
        }
    }

    #[test]
    fn bash_tool_use_then_result_roundtrip() {
        let mut m = Mapper::new();
        let started = feed(
            &mut m,
            r#"{"type":"assistant","message":{"id":"msg_3","content":[{"type":"tool_use","id":"toolu_bash","name":"Bash","input":{"command":"echo hi"}}]}}"#,
        );
        match &started[0] {
            AgentEvent::ItemStarted(item) => {
                assert_eq!(item.id, "toolu_bash");
                match &item.content {
                    ItemContent::CommandExecution {
                        command, status, ..
                    } => {
                        assert_eq!(command, "echo hi");
                        assert_eq!(*status, ItemStatus::InProgress);
                    }
                    other => panic!("expected CommandExecution, got {other:?}"),
                }
            }
            other => panic!("expected ItemStarted, got {other:?}"),
        }

        let done = feed(
            &mut m,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_bash","content":"hi\n"}]}}"#,
        );
        match &done[0] {
            AgentEvent::ItemCompleted(item) => match &item.content {
                ItemContent::CommandExecution {
                    output,
                    status,
                    exit_code,
                    ..
                } => {
                    assert_eq!(output, "hi\n");
                    assert_eq!(*status, ItemStatus::Completed);
                    assert_eq!(*exit_code, Some(0));
                }
                other => panic!("expected CommandExecution, got {other:?}"),
            },
            other => panic!("expected ItemCompleted, got {other:?}"),
        }
    }

    #[test]
    fn reinvocation_after_background_task_synthesizes_turn() {
        let mut m = Mapper::new();
        feed(
            &mut m,
            r#"{"type":"system","subtype":"init","session_id":"session-bg","model":"claude-haiku-4-5-20251001"}"#,
        );
        let first_turn_id = m.start_turn();

        let started = feed(
            &mut m,
            r#"{"type":"assistant","message":{"id":"msg-bg","content":[{"type":"tool_use","id":"toolu-bg","name":"Bash","input":{"command":"sleep 8 && echo BG_DONE","description":"Launch background sleep command","run_in_background":true}}]}}"#,
        );
        assert!(matches!(
            started.as_slice(),
            [AgentEvent::ItemStarted(ThreadItem {
                content: ItemContent::CommandExecution {
                    status: ItemStatus::InProgress,
                    ..
                },
                ..
            })]
        ));
        let count = feed(
            &mut m,
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"bg-1","task_type":"local_bash","description":"Launch background sleep command"}]}"#,
        );
        assert!(matches!(
            count.as_slice(),
            [AgentEvent::BackgroundTasksChanged { count: 1 }]
        ));
        let task_started = feed(
            &mut m,
            r#"{"type":"system","subtype":"task_started","task_id":"bg-1","tool_use_id":"toolu-bg","description":"Launch background sleep command","task_type":"local_bash"}"#,
        );
        assert!(task_started.is_empty());
        let running = feed(
            &mut m,
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu-bg","type":"tool_result","content":"Command running in background with ID: bg-1","is_error":false}]},"tool_use_result":{"stdout":"","stderr":"","interrupted":false,"backgroundTaskId":"bg-1"}}"#,
        );
        assert!(matches!(
            running.as_slice(),
            [AgentEvent::ItemUpdated(ThreadItem {
                content: ItemContent::CommandExecution {
                    status: ItemStatus::InProgress,
                    exit_code: None,
                    ..
                },
                ..
            })]
        ));
        let first_result = feed(
            &mut m,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Background command launched."}"#,
        );
        assert!(matches!(
            turn_completed_after_count(&first_result, 1),
            AgentEvent::TurnCompleted {
                turn_id,
                status: TurnStatus::Completed,
                ..
            } if turn_id == &first_turn_id
        ));

        assert!(
            feed(
                &mut m,
                r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#,
            )
            .is_empty()
        );
        assert!(
            feed(
                &mut m,
                r#"{"type":"system","subtype":"task_updated","task_id":"bg-1","patch":{"status":"completed","end_time":1784396058581}}"#,
            )
            .is_empty()
        );
        let notification = feed(
            &mut m,
            r#"{"type":"system","subtype":"task_notification","task_id":"bg-1","tool_use_id":"toolu-bg","status":"completed","output_file":"/tmp/bg-1.output","summary":"Background command completed (exit code 0)"}"#,
        );
        assert_eq!(notification.len(), 1);
        assert!(matches!(
            &notification[0],
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::CommandExecution {
                    status: ItemStatus::Completed,
                    exit_code: Some(0),
                    ..
                },
                ..
            })
        ));
        let reinit = feed(
            &mut m,
            r#"{"type":"system","subtype":"init","session_id":"session-bg","model":"claude-haiku-4-5-20251001"}"#,
        );
        assert!(!reinit.iter().any(|event| matches!(
            event,
            AgentEvent::SessionStarted { .. }
                | AgentEvent::TurnStarted { .. }
                | AgentEvent::TurnCompleted { .. }
        )));

        let reinvoked = feed(
            &mut m,
            r#"{"type":"assistant","message":{"id":"msg-reinvoked","content":[{"type":"text","text":"Background command completed successfully."}]}}"#,
        );
        let synthesized_turn_id = match &reinvoked[0] {
            AgentEvent::TurnStarted { turn_id } => turn_id.clone(),
            other => panic!("expected synthesized TurnStarted first, got {other:?}"),
        };
        assert_ne!(synthesized_turn_id, first_turn_id);
        assert!(matches!(
            &reinvoked[1],
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::AssistantMessage { text },
                ..
            }) if text == "Background command completed successfully."
        ));

        let second_result = feed(
            &mut m,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Background command completed successfully.","origin":{"kind":"task-notification"}}"#,
        );
        assert!(matches!(
            &second_result[0],
            AgentEvent::BackgroundTasksChanged { count: 0 }
        ));
        assert!(matches!(
            &second_result[1],
            AgentEvent::TurnCompleted {
                turn_id,
                status: TurnStatus::Completed,
                ..
            } if turn_id == &synthesized_turn_id
        ));
    }

    /// The zero-count publication must not depend on the CLI's `origin.kind`
    /// shape: after `background_tasks_changed []` (whose zero is deliberately
    /// withheld), ANY later result must tell the runtime the tasks are gone —
    /// otherwise the session sits at "Working" forever on a live idle process.
    #[test]
    fn any_result_publishes_zero_after_background_tasks_drain() {
        let mut m = Mapper::new();
        m.start_turn();
        feed(
            &mut m,
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"bg-1","task_type":"local_bash","description":"sleep"}]}"#,
        );
        let first = feed(
            &mut m,
            r#"{"type":"result","subtype":"success","is_error":false}"#,
        );
        turn_completed_after_count(&first, 1);

        // The drain event withholds its zero (parking guard)...
        assert!(
            feed(
                &mut m,
                r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#,
            )
            .is_empty()
        );
        // ...so a later ordinary result — no `origin` at all — must publish it.
        m.start_turn();
        let second = feed(
            &mut m,
            r#"{"type":"result","subtype":"success","is_error":false}"#,
        );
        turn_completed_after_count(&second, 0);
    }

    /// Every task the CLI lists keeps the process busy, not only background
    /// Bash: a backgrounded Agent (`task_type: local_agent`, as the CLI
    /// reports it) must hold the count above zero until its notification's
    /// follow-up result, or the runtime judges the session quiescent while
    /// the subagent is still running. Shapes recorded from Claude Code 2.1.301.
    #[test]
    fn non_bash_background_tasks_count_until_their_follow_up_result() {
        let mut m = Mapper::new();
        m.start_turn();
        feed(
            &mut m,
            r#"{"type":"assistant","message":{"id":"msg-spawn","content":[{"type":"tool_use","id":"toolu_agent","name":"Agent","input":{"description":"Run sleep and echo command","subagent_type":"general-purpose","run_in_background":true}}]}}"#,
        );
        let counted = feed(
            &mut m,
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"ae5779035aeb8a94d","run_id":"0mvctgbkr-7639c0ca","task_type":"local_agent","subagent_type":"general-purpose","description":"Run sleep and echo command"}]}"#,
        );
        assert!(matches!(
            counted.as_slice(),
            [AgentEvent::BackgroundTasksChanged { count: 1 }]
        ));
        feed(
            &mut m,
            r#"{"type":"system","subtype":"task_started","task_id":"ae5779035aeb8a94d","tool_use_id":"toolu_agent","description":"Run sleep and echo command","subagent_type":"general-purpose","is_backgrounded":true,"task_type":"local_agent"}"#,
        );
        feed(
            &mut m,
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_agent","type":"tool_result","content":[{"type":"text","text":"Async agent launched successfully."}]}]},"tool_use_result":{"isAsync":true,"status":"async_launched","agentId":"ae5779035aeb8a94d"}}"#,
        );
        let launched = feed(
            &mut m,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"launched"}"#,
        );
        turn_completed_after_count(&launched, 1);

        // Between turns, the background subagent asks for permission.
        let asked = feed(
            &mut m,
            r#"{"type":"control_request","request_id":"ctrl-bg","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"cp a b"},"tool_use_id":"toolu_cp","agent_id":"ae5779035aeb8a94d"}}"#,
        );
        let [AgentEvent::ApprovalRequested(request)] = asked.as_slice() else {
            panic!("expected one approval request, got {asked:?}");
        };
        assert_eq!(request.parent_item_id.as_deref(), Some("toolu_agent"));

        let notification = feed(
            &mut m,
            r#"{"type":"system","subtype":"task_notification","task_id":"ae5779035aeb8a94d","tool_use_id":"toolu_agent","status":"completed","output_file":"/tmp/ae5779035aeb8a94d.output","summary":"SUB_DONE"}"#,
        );
        assert!(
            !notification
                .iter()
                .any(|event| matches!(event, AgentEvent::ItemCompleted(_))),
            "a subagent task is settled as a Subagent, never as a command card: {notification:?}"
        );
        assert!(
            feed(
                &mut m,
                r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#,
            )
            .is_empty()
        );
        m.start_turn();
        let done = feed(
            &mut m,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"DONE","origin":{"kind":"task-notification","producer":"session-task","runId":"0mvctgbkr-7639c0ca"}}"#,
        );
        turn_completed_after_count(&done, 0);
    }

    #[test]
    fn unknown_task_notification_without_tool_use_id_does_not_wedge_turn() {
        let mut m = Mapper::new();
        let turn_id = m.start_turn();

        assert!(
            feed(
                &mut m,
                r#"{"type":"system","subtype":"task_notification","task_id":"unknown-task","status":"completed"}"#,
            )
            .is_empty()
        );
        let result = feed(
            &mut m,
            r#"{"type":"result","subtype":"success","is_error":false}"#,
        );
        assert!(matches!(
            turn_completed_after_count(&result, 0),
            AgentEvent::TurnCompleted {
                turn_id: completed,
                status: TurnStatus::Completed,
                ..
            } if completed == &turn_id
        ));
    }

    #[test]
    fn classification_matrix_covers_substring_priority() {
        use ClaudeRequestType::*;
        let cases = [
            ("Read", FileRead),       // exact lowercase "read"
            ("Read File", FileRead),  // "read file" substring
            ("ReadFile", FileChange), // no space → "file" classifies it as file_change
            ("View", FileRead),
            ("ViewImage", FileRead), // "view" wins before "image"
            ("Grep", FileRead),
            ("Glob", FileRead),
            ("WebSearch", FileRead), // "search" predicate wins over web_search
            ("codebase_search", FileRead),
            ("WebFetch", ToolUse), // neither search nor read-only recognizes it
            ("Bash", ExecCommand),
            ("run_shell", ExecCommand),
            ("terminal", ExecCommand),
            ("some_command", ExecCommand),
            ("Edit", FileChange),
            ("Write", FileChange),
            ("MultiEdit", FileChange),
            ("delete_thing", FileChange),
            ("TodoWrite", FileChange),  // "write"
            ("TaskCreate", FileChange), // "create"
            ("TaskUpdate", ToolUse),    // no classification substring
            ("TaskList", ToolUse),
            ("Task", ToolUse), // agent item, falls through
            ("some_agent", ToolUse),
            ("subagent_run", ToolUse),
            ("mcp__server__tool", ToolUse),
            ("view_image", FileRead), // "view" still wins
            ("image_tool", ToolUse),  // image → dynamic
            ("MysteryTool", ToolUse),
        ];
        for (name, expected) in cases {
            assert_eq!(classify_claude_tool(name), expected, "classifying {name:?}");
        }
    }

    #[test]
    fn approval_details_reach_native_permission_requests() {
        let mut m = Mapper::new();
        m.start_turn();
        for (tool, input, expected) in [
            (
                "Task",
                json!({"subagent_type":"explore", "description":"find refs", "prompt":"ignored"}),
                Some("explore: find refs"),
            ),
            (
                "Task",
                json!({"prompt":"find references"}),
                Some("find references"),
            ),
            ("Weird", json!({"a":1}), Some("Weird: {\"a\":1}")),
            (
                "Read",
                json!({"file_path":"/tmp/a.txt"}),
                Some("Read: {\"file_path\":\"/tmp/a.txt\"}"),
            ),
            ("Task", json!({"prompt":"y".repeat(300)}), None),
            ("Weird", json!({"blob":"z".repeat(500)}), None),
        ] {
            let events = m.on_message(json!({
                "type": "control_request",
                "request_id": "detail-request",
                "request": {"subtype":"can_use_tool", "tool_name":tool, "input":input},
            }));
            let [AgentEvent::ApprovalRequested(request)] = events.as_slice() else {
                panic!("expected native permission request, got {events:?}");
            };
            let detail = match &request.kind {
                ApprovalKind::ToolUse {
                    name,
                    input: actual_input,
                    detail,
                } => {
                    assert_eq!(name, tool);
                    assert_eq!(actual_input, &input);
                    detail
                }
                ApprovalKind::FileRead { detail } if tool == "Read" => detail,
                other => panic!("unexpected approval for {tool}: {other:?}"),
            };
            if let Some(expected) = expected {
                assert_eq!(detail, expected);
            } else if tool == "Task" {
                let prompt = input["prompt"].as_str().unwrap();
                assert!(!detail.is_empty());
                assert!(prompt.starts_with(detail.as_str()));
                assert!(
                    detail.len() < prompt.len(),
                    "long task prompts must remain bounded"
                );
            } else {
                assert!(detail.starts_with("Weird: {\"blob\":\""));
                assert!(detail.ends_with("..."));
                assert!(detail.len() < input["blob"].as_str().unwrap().len());
            }
        }
    }

    #[test]
    fn ask_user_question_parse_and_answer_wire_shape() {
        let mut m = Mapper::new();
        feed(
            &mut m,
            r#"{"type":"control_request","request_id":"ctrl-x","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"question":"q?","header":"h"}]}}}"#,
        );
        let cancels = m.cancel_pending_user_input();
        assert_eq!(cancels.len(), 1);
        let (id, resp) = &cancels[0];
        assert_eq!(id, "ctrl-x");
        assert_eq!(resp["response"]["response"]["behavior"], "deny");
        assert_eq!(
            resp["response"]["response"]["message"],
            "User cancelled tool execution."
        );
        // Drained: no longer answerable.
        let empty = serde_json::Map::new();
        assert!(m.build_user_input_response("ctrl-x", &empty).is_none());

        let mut m = Mapper::new();
        m.start_turn();
        let evs = feed(
            &mut m,
            r#"{"type":"control_request","request_id":"ctrl-9","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"question":"Which color?","header":"Color","options":[{"label":"Red","description":"warm"},{"label":"Blue","description":""}],"multiSelect":false},{"header":"Free"}]}}}"#,
        );
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            AgentEvent::UserInputRequested {
                request_id,
                questions,
                ..
            } => {
                assert_eq!(request_id, "ctrl-9");
                assert_eq!(questions.len(), 2);
                // id = complete question text.
                assert_eq!(questions[0].id, "Which color?");
                assert_eq!(questions[0].header, "Color");
                assert_eq!(questions[0].options.len(), 2);
                assert_eq!(questions[0].options[0].label, "Red");
                assert_eq!(questions[0].options[1].description, "");
                assert!(!questions[0].multi_select);
                // Missing question text → id fallback q-<index>, header kept.
                assert_eq!(questions[1].id, "q-1");
                assert_eq!(questions[1].header, "Free");
                assert!(questions[1].options.is_empty());
            }
            other => panic!("expected UserInputRequested, got {other:?}"),
        }

        // Answer: allow with {questions: <original>, answers: <provided>}.
        let mut answers = serde_json::Map::new();
        answers.insert("Which color?".into(), json!("Red"));
        let resp = m
            .build_user_input_response("ctrl-9", &answers)
            .expect("response for known request");
        assert_eq!(resp["response"]["subtype"], "success");
        assert_eq!(resp["response"]["request_id"], "ctrl-9");
        assert_eq!(resp["response"]["response"]["behavior"], "allow");
        assert_eq!(
            resp["response"]["response"]["updatedInput"]["answers"]["Which color?"],
            "Red"
        );
        // Original questions echoed back verbatim.
        assert_eq!(
            resp["response"]["response"]["updatedInput"]["questions"][0]["header"],
            "Color"
        );
        // Consumed once.
        assert!(m.build_user_input_response("ctrl-9", &answers).is_none());
    }

    #[test]
    fn result_maps_to_turn_completed_with_usage() {
        let mut m = Mapper::new();
        let turn_id = m.start_turn();
        let evs = feed(
            &mut m,
            r#"{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.125,"duration_ms":4321,"usage":{"input_tokens":100,"cache_read_input_tokens":50,"cache_creation_input_tokens":10,"output_tokens":20},"modelUsage":{"claude-opus-4-8[1m]":{"contextWindow":1000000}}}"#,
        );
        match turn_completed_after_count(&evs, 0) {
            AgentEvent::TurnCompleted {
                turn_id: tid,
                status,
                usage,
            } => {
                assert_eq!(tid, &turn_id);
                assert_eq!(*status, TurnStatus::Completed);
                let usage = usage.as_ref().expect("usage present");
                assert_eq!(usage.input_tokens, None);
                assert_eq!(usage.cached_input_tokens, None);
                assert_eq!(usage.output_tokens, None);
                assert_eq!(usage.used_tokens, None);
                assert_eq!(usage.turn_processed_tokens, Some(180));
                assert_eq!(usage.total_processed_tokens, None);
                assert_eq!(usage.context_window, Some(1_000_000));
                assert_eq!(usage.cost_usd, Some(0.125));
                assert_eq!(usage.duration_ms, Some(4321));
            }
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
    }

    #[test]
    fn interrupted_result_status() {
        let mut mapper = Mapper::new();
        mapper.start_turn();
        let events = feed(
            &mut mapper,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"You can cancel the operation from Settings."}"#,
        );
        assert!(matches!(
            turn_completed_after_count(&events, 0),
            AgentEvent::TurnCompleted {
                status: TurnStatus::Completed,
                ..
            }
        ));

        let mut idle = Mapper::new();
        idle.interrupt_request();
        assert!(!idle.interrupt_pending);
        let idle_result = feed(
            &mut idle,
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"provider failed"}"#,
        );
        // A failed turn discloses the CLI's raw result text before the marker.
        assert!(matches!(
            &idle_result[0],
            AgentEvent::Error { message, fatal: false }
                if message == "claude turn failed (error_during_execution): provider failed"
        ));
        assert!(matches!(
            turn_completed_after_count(&idle_result, 0),
            AgentEvent::TurnCompleted {
                status: TurnStatus::Failed,
                ..
            }
        ));

        let mut m = Mapper::new();
        m.start_turn();
        m.interrupt_request();
        assert!(m.interrupt_pending);
        let attributed = feed(
            &mut m,
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"provider failed"}"#,
        );
        assert!(matches!(
            turn_completed_after_count(&attributed, 0),
            AgentEvent::TurnCompleted {
                status: TurnStatus::Interrupted,
                ..
            }
        ));

        m.start_turn();
        let evs = feed(
            &mut m,
            r#"{"type":"result","subtype":"error_during_execution","is_error":false,"result":"Request was aborted"}"#,
        );
        match turn_completed_after_count(&evs, 0) {
            AgentEvent::TurnCompleted { status, .. } => {
                assert_eq!(*status, TurnStatus::Interrupted)
            }
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
    }

    #[test]
    fn full_fixture_trace_parses() {
        let trace = include_str!("../tests/fixtures/claude/tool_use_trace.jsonl");
        let mut m = Mapper::new();
        let mut all = Vec::new();
        let mut turn_id = None;
        let mut approvals = 0;
        for line in trace.lines().filter(|line| !line.trim().is_empty()) {
            let msg: Value = serde_json::from_str(line).expect("fixture line is json");
            for event in m.on_message(msg) {
                if let AgentEvent::TurnStarted { turn_id: started } = &event {
                    turn_id = Some(started.clone());
                }
                if let AgentEvent::ApprovalRequested(request) = &event {
                    approvals += 1;
                    assert_eq!(request.id, "221a98ab-3314-470d-89e0-0528a9e8d75f");
                    assert!(turn_id.is_some());
                    assert_eq!(request.turn_id, turn_id);
                    let ApprovalKind::FileChange { changes, reason } = &request.kind else {
                        panic!("expected Write approval, got {request:?}");
                    };
                    assert_eq!(changes.len(), 1);
                    assert_eq!(changes[0].path, "/private/tmp/probe-claude-py/hello.txt");
                    assert_eq!(reason.as_deref(), Some("hello.txt"));
                    let response = m
                        .build_approval_response(
                            &request.id,
                            ApprovalDecision::Option("allow".into()),
                        )
                        .expect("response to the recorded request");
                    assert_eq!(
                        response,
                        json!({
                            "type": "control_response",
                            "response": {
                                "subtype": "success",
                                "request_id": "221a98ab-3314-470d-89e0-0528a9e8d75f",
                                "response": {
                                    "behavior": "allow",
                                    "updatedInput": {
                                        "file_path": "/private/tmp/probe-claude-py/hello.txt",
                                        "content": "hi\n",
                                    },
                                },
                            },
                        })
                    );
                    assert!(
                        m.build_approval_response(
                            &request.id,
                            ApprovalDecision::Option("allow".into())
                        )
                        .is_none()
                    );
                }
                all.push(event);
            }
        }
        assert!(
            all.iter()
                .any(|event| matches!(event, AgentEvent::SessionStarted { .. })),
            "expected SessionStarted"
        );
        assert_eq!(approvals, 1);

        // Empty structuredPatch in the recorded result must preserve the Write input's diff.
        let changes: Vec<_> = all
            .iter()
            .filter_map(|event| {
                let (phase, item) = match event {
                    AgentEvent::ItemStarted(item) => ("started", item),
                    AgentEvent::ItemCompleted(item) => ("completed", item),
                    _ => return None,
                };
                let ItemContent::FileChange { changes, status } = &item.content else {
                    return None;
                };
                Some((phase, item.id.as_str(), *status, changes.as_slice()))
            })
            .collect();
        let expected = [FileChange {
            path: "/private/tmp/probe-claude-py/hello.txt".into(),
            kind: FileChangeKind::Create,
            diff: Some("+hi".into()),
        }];
        assert_eq!(
            changes,
            vec![
                (
                    "started",
                    "toolu_01E6rT4J5qJva9HnEfBx5top",
                    ItemStatus::InProgress,
                    expected.as_slice()
                ),
                (
                    "completed",
                    "toolu_01E6rT4J5qJva9HnEfBx5top",
                    ItemStatus::Completed,
                    expected.as_slice()
                ),
            ]
        );
        assert!(
            all.iter().any(|event| matches!(
                event,
                AgentEvent::TurnCompleted {
                    status: TurnStatus::Completed,
                    ..
                }
            )),
            "expected completed TurnCompleted"
        );
    }

    #[test]
    fn subagent_fixture_maps_lifecycle_and_parented_activity() {
        let trace = include_str!("../tests/fixtures/claude/subagent_trace.jsonl");
        let mut mapper = Mapper::new();
        let mut events = Vec::new();
        for line in trace.lines() {
            events.extend(feed(&mut mapper, line));
        }
        // task_started opened a transcript tail; every terminal snapshot
        // (task_updated, task_notification, tool_result) waits for its ack.
        assert!(events.iter().all(|event| !matches!(
            event,
            AgentEvent::ItemUpdated(ThreadItem { content: ItemContent::Subagent { status, .. }, .. })
            | AgentEvent::ItemCompleted(ThreadItem { content: ItemContent::Subagent { status, .. }, .. })
                if *status != ItemStatus::InProgress
        )));
        events.extend(mapper.on_tail_notice(SubagentTailNotice {
            parent_id: "toolu_spawn_1".into(),
            events: Vec::new(),
            model: None,
            stopped: true,
        }));

        let spawn_events: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ItemStarted(item)
                | AgentEvent::ItemUpdated(item)
                | AgentEvent::ItemCompleted(item)
                    if item.id == "toolu_spawn_1" =>
                {
                    Some(item)
                }
                _ => None,
            })
            .collect();
        assert_eq!(spawn_events.len(), 4);
        assert!(matches!(events.last(), Some(AgentEvent::ItemCompleted(_))));
        assert!(matches!(
            &spawn_events[0].content,
            ItemContent::Subagent { agent_type, description, status: ItemStatus::InProgress, summary: None, model: None, effort: None }
                if agent_type == "general-purpose" && description == "Ping test"
        ));
        // The child's first assistant message reveals the model and effort the
        // subagent actually ran with; the spawn item re-emits with them.
        assert!(matches!(
            &spawn_events[2].content,
            ItemContent::Subagent { status: ItemStatus::InProgress, model: Some(model), effort: Some(effort), .. }
                if model == "claude-sonnet-5" && effort == "high"
        ));
        assert!(matches!(
            &spawn_events.last().unwrap().content,
            ItemContent::Subagent { model: Some(model), effort: Some(effort), .. }
                if model == "claude-sonnet-5" && effort == "high"
        ));

        assert!(spawn_events.iter().any(|item| matches!(
            &item.content,
            ItemContent::Subagent { status: ItemStatus::Completed, summary: Some(summary), .. }
                if summary == "pong"
        )));
        assert!(matches!(
            &spawn_events.last().unwrap().content,
            ItemContent::Subagent { status: ItemStatus::Completed, summary: Some(summary), .. }
                if summary == "pong"
        ));

        let children: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ItemStarted(item)
                | AgentEvent::ItemUpdated(item)
                | AgentEvent::ItemCompleted(item)
                    if item.parent_item_id.as_deref() == Some("toolu_spawn_1") =>
                {
                    Some(item)
                }
                _ => None,
            })
            .collect();
        assert_eq!(children.len(), 2);
        assert!(matches!(
            &children[0].content,
            ItemContent::UserMessage { text, .. } if text.contains("Reply with pong")
        ));
        assert!(matches!(
            &children[1].content,
            ItemContent::AssistantMessage { text } if text == "pong"
        ));

        assert!(events.iter().all(|event| match event {
            AgentEvent::ItemStarted(item)
            | AgentEvent::ItemUpdated(item)
            | AgentEvent::ItemCompleted(item) => {
                item.id == "toolu_spawn_1" || item.parent_item_id.is_some()
            }
            _ => true,
        }));
    }

    /// Background subagent: the launch acknowledgement and the parent's result
    /// leave the Subagent item in progress; the terminal snapshot is emitted
    /// only after the transcript tail's final flush, and the tail's copy of a
    /// child the stdout feed already completed cannot reopen it.
    #[test]
    fn background_subagent_terminal_waits_for_tail_flush_and_dedupes_feeds() {
        let trace = include_str!("../tests/fixtures/claude/subagent_background_trace.jsonl");
        let mut mapper = Mapper::new();
        let mut events = Vec::new();
        for line in trace.lines() {
            events.extend(feed(&mut mapper, line));
        }
        let spawn_status = |event: &AgentEvent| match event {
            AgentEvent::ItemStarted(item)
            | AgentEvent::ItemUpdated(item)
            | AgentEvent::ItemCompleted(item)
                if item.id == "toolu_spawn_bg" =>
            {
                match &item.content {
                    ItemContent::Subagent { status, .. } => Some(*status),
                    _ => None,
                }
            }
            _ => None,
        };
        assert!(
            events
                .iter()
                .filter_map(spawn_status)
                .all(|status| status == ItemStatus::InProgress),
            "launch acknowledgement and task_notification must not settle the item yet"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::TurnCompleted { .. }))
        );
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ItemCompleted(ThreadItem { id, content: ItemContent::ToolCall { status: ItemStatus::Completed, .. }, .. })
                if id == "toolu_spawn_bg:toolu_child_grep"
        )));
        assert!(matches!(
            mapper.take_tail_requests().as_slice(),
            [
                TailRequest::Start { parent_id: start, .. },
                TailRequest::PreferPath { parent_id: prefer, .. },
                TailRequest::Stop { parent_id: stop },
            ] if start == "toolu_spawn_bg" && prefer == "toolu_spawn_bg" && stop == "toolu_spawn_bg"
        ));

        // The tail maps the same transcript records a second time, plus one
        // record the stdout feed never carried.
        let mut tail = crate::subagent_tail::TranscriptMapper::new("toolu_spawn_bg");
        let mut tail_events = Vec::new();
        for line in trace
            .lines()
            .filter(|line| line.contains("parent_tool_use_id"))
        {
            tail_events.extend(tail.map_value(&serde_json::from_str(line).unwrap()));
        }
        tail_events.extend(tail.map_value(&json!({
            "type": "assistant", "message": { "id": "msg-child-2", "content": [{ "type": "text", "text": "done" }] }
        })));
        let flushed = mapper.on_tail_notice(SubagentTailNotice {
            parent_id: "toolu_spawn_bg".into(),
            events: tail_events,
            model: None,
            stopped: true,
        });
        assert!(
            flushed
                .iter()
                .all(|event| !matches!(event, AgentEvent::ItemStarted(_))),
            "a completed child must not be restarted by the tail's copy"
        );
        assert!(matches!(
            flushed.last(),
            Some(AgentEvent::ItemUpdated(ThreadItem { id, content: ItemContent::Subagent { status: ItemStatus::Completed, summary: Some(summary), model: Some(model), effort: Some(effort), .. }, .. }))
                if id == "toolu_spawn_bg" && summary == "Routing audited" && model == "claude-sonnet-5" && effort == "high"
        ));
        assert!(matches!(
            &flushed[flushed.len() - 2],
            AgentEvent::ItemCompleted(ThreadItem { id, content: ItemContent::AssistantMessage { text }, .. })
                if id == "toolu_spawn_bg:msg-child-2:0" && text == "done"
        ));
        assert!(mapper.take_pending_subagent_terminals().is_empty());
    }

    #[test]
    fn resumed_subagent_keeps_its_spawn_identity_across_parent_restart() {
        smol::block_on(async {
            let root =
                std::env::temp_dir().join(format!("tcode-resumed-agent-{}", uuid::Uuid::new_v4()));
            let dir = root.join("projects/project/session/subagents");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("agent-child.meta.json"),
                r#"{"agentType":"general-purpose","description":"Audit routing","toolUseId":"spawn","spawnDepth":1,"requestShape":"background","requestNonInteractive":true}"#
            ).unwrap();
            for restart in [true, false] {
                let mut mapper = Mapper::new();
                if !restart {
                    feed(
                        &mut mapper,
                        r#"{"type":"assistant","message":{"id":"initial","content":[{"type":"tool_use","id":"spawn","name":"Agent","input":{"description":"Audit routing","subagent_type":"general-purpose"}}]}}"#,
                    );
                    feed(
                        &mut mapper,
                        r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"spawn","content":"Launched"}]},"tool_use_result":{"status":"async_launched","agentId":"child"}}"#,
                    );
                    feed(
                        &mut mapper,
                        r#"{"type":"system","subtype":"task_notification","task_id":"child","tool_use_id":"spawn","status":"completed"}"#,
                    );
                }
                let mut events = Vec::new();
                for msg in [
                    json!({"type":"assistant","message":{"id":"resume-message","content":[{"type":"tool_use","id":"send","name":"SendMessage","input":{"to":"child","message":"Continue the audit"}}]}}),
                    json!({"type":"user","session_id":"session","message":{"content":[{"type":"tool_result","tool_use_id":"send","content":[{"type":"text","text":"{\"success\":true,\"resumedAgentId\":\"child\"}"}]}]},"tool_use_result":{"success":true,"resumedAgentId":"child"}}),
                ] {
                    events.extend(
                        mapper
                            .on_wire_message(msg, restart.then_some(root.as_path()))
                            .await,
                    );
                }
                events.extend(mapper.on_wire_message(json!({"type":"system","subtype":"task_notification","session_id":"session","task_id":"child","tool_use_id":"send","status":"completed","summary":"Audit complete"}), restart.then_some(root.as_path())).await);

                assert!(events.iter().any(|event| matches!(event,
                    AgentEvent::ItemUpdated(ThreadItem { id, content: ItemContent::Subagent {status: ItemStatus::Completed, ..}, .. }) if id == "spawn"
                )), "completion must settle the original mirror (restart={restart})");
                assert!(events.iter().any(|event| matches!(event,
                    AgentEvent::ItemUpdated(ThreadItem { id, content: ItemContent::Subagent {status: ItemStatus::InProgress, ..}, .. }) if id == "spawn"
                )), "resuming must reopen the original mirror (restart={restart})");
                assert!(events.iter().any(|event| matches!(event,
                    AgentEvent::ItemCompleted(ThreadItem { id, content: ItemContent::ToolCall {status: ItemStatus::Completed, ..}, .. }) if id == "send"
                )), "SendMessage must retain its own tool result");
            }
            std::fs::remove_dir_all(root).unwrap();
        });
    }

    /// A background subagent that itself spawns a background Agent: the
    /// nested spawn is a Subagent item parented to the child, the
    /// grandchild's transcript is parented to the nested spawn, and the
    /// grandchild's task_notification settles it after its tail's final
    /// flush. Both feeds carry every child record; each stage is emitted once.
    #[test]
    fn nested_background_subagent_settles_through_its_own_task_notification() {
        let trace = include_str!("../tests/fixtures/claude/subagent_nested_trace.jsonl");
        let mut mapper = Mapper::new();
        let mut events = Vec::new();
        for line in trace.lines() {
            events.extend(feed(&mut mapper, line));
        }
        let subagent_snapshots = |events: &[AgentEvent], id: &str| -> Vec<ThreadItem> {
            events
                .iter()
                .filter_map(|event| match event {
                    AgentEvent::ItemStarted(item)
                    | AgentEvent::ItemUpdated(item)
                    | AgentEvent::ItemCompleted(item)
                        if item.id == id
                            && matches!(item.content, ItemContent::Subagent { .. }) =>
                    {
                        Some(item.clone())
                    }
                    _ => None,
                })
                .collect()
        };

        let grandchild = subagent_snapshots(&events, "toolu_grandchild");
        assert!(matches!(
            &grandchild[0],
            ThreadItem { parent_item_id: Some(parent), content: ItemContent::Subagent { agent_type, description, status: ItemStatus::InProgress, .. }, .. }
                if parent == "toolu_child" && agent_type == "general-purpose" && description == "Research Workers limits"
        ));
        assert_eq!(
            grandchild
                .iter()
                .filter(|item| matches!(
                    item.content,
                    ItemContent::Subagent {
                        status: ItemStatus::InProgress,
                        ..
                    }
                ))
                .count(),
            grandchild.len(),
            "launch acknowledgement and task_notification wait for the grandchild's tail"
        );
        assert!(
            grandchild
                .iter()
                .all(|item| item.parent_item_id.as_deref() == Some("toolu_child"))
        );
        assert!(grandchild.iter().any(|item| matches!(
            &item.content,
            ItemContent::Subagent { model: Some(model), effort: Some(effort), .. }
                if model == "claude-sonnet-5" && effort == "high"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ItemCompleted(ThreadItem { id, parent_item_id: Some(parent), content: ItemContent::ToolCall { name, status: ItemStatus::Completed, .. }, .. })
                if id == "toolu_grandchild:toolu_gc_fetch" && parent == "toolu_grandchild" && name == "WebFetch"
        )));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::ItemStarted(ThreadItem { id, .. }) if id == "toolu_grandchild"))
                .count(),
            1
        );
        let requests = mapper.take_tail_requests();
        assert!(requests.iter().any(|request| matches!(request, TailRequest::Start { parent_id, .. } if parent_id == "toolu_grandchild")));
        assert!(requests.iter().any(|request| matches!(request, TailRequest::Stop { parent_id } if parent_id == "toolu_grandchild")));

        // The child's tail replays the nested spawn and the grandchild's tail
        // replays its transcript; neither reopens or repeats a delivered stage.
        let mut child_tail = crate::subagent_tail::TranscriptMapper::new("toolu_child");
        let mut grandchild_tail = crate::subagent_tail::TranscriptMapper::new("toolu_grandchild");
        let mut child_events = Vec::new();
        let mut grandchild_events = Vec::new();
        for line in trace.lines() {
            let value: Value = serde_json::from_str(line).unwrap();
            match value.get("parent_tool_use_id").and_then(Value::as_str) {
                Some("toolu_child") => child_events.extend(child_tail.map_value(&value)),
                Some("toolu_grandchild") => {
                    grandchild_events.extend(grandchild_tail.map_value(&value))
                }
                _ => {}
            }
        }
        let flushed = mapper.on_tail_notice(SubagentTailNotice {
            parent_id: "toolu_grandchild".into(),
            events: grandchild_events,
            model: None,
            stopped: true,
        });
        assert!(
            matches!(
                flushed.as_slice(),
                [AgentEvent::ItemUpdated(ThreadItem { id, parent_item_id: Some(parent), content: ItemContent::Subagent { status: ItemStatus::Completed, summary: Some(summary), model: Some(model), effort: Some(effort), .. }, .. })]
                    if id == "toolu_grandchild" && parent == "toolu_child" && summary == "Agent \"Research Workers limits\" finished" && model == "claude-sonnet-5" && effort == "high"
            ),
            "{flushed:?}"
        );
        let flushed = mapper.on_tail_notice(SubagentTailNotice {
            parent_id: "toolu_child".into(),
            events: child_events,
            model: None,
            stopped: true,
        });
        assert!(
            matches!(
                flushed.as_slice(),
                [AgentEvent::ItemUpdated(ThreadItem { id, parent_item_id: None, content: ItemContent::Subagent { status: ItemStatus::Completed, summary: Some(summary), .. }, .. })]
                    if id == "toolu_child" && summary == "Hosting researched"
            ),
            "{flushed:?}"
        );
        assert!(mapper.take_pending_subagent_terminals().is_empty());
    }

    /// A nested Agent that runs in the foreground settles with the child's
    /// tool_result; a coordination tool is not a spawn.
    #[test]
    fn nested_foreground_subagent_completes_on_the_childs_tool_result() {
        let mut mapper = Mapper::new();
        let mut events = feed(
            &mut mapper,
            r#"{"type":"assistant","message":{"id":"msg-spawn","content":[{"type":"tool_use","id":"toolu_child","name":"Agent","input":{"description":"Audit","prompt":"Audit routing.","subagent_type":"Explore"}}]}}"#,
        );
        events.extend(feed(
            &mut mapper,
            r#"{"type":"assistant","parent_tool_use_id":"toolu_child","message":{"id":"msg-child-1","content":[{"type":"tool_use","id":"toolu_list","name":"ListAgents","input":{}},{"type":"tool_use","id":"toolu_nested","name":"Agent","input":{"description":"Check tests","prompt":"Check the tests.","subagent_type":"Explore"}}]}}"#,
        ));
        events.extend(feed(
            &mut mapper,
            r#"{"type":"user","parent_tool_use_id":"toolu_child","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_list","content":"none"},{"type":"tool_result","tool_use_id":"toolu_nested","content":[{"type":"text","text":"Tests   are\ngreen."}]}]}}"#,
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ItemStarted(ThreadItem { id, parent_item_id: Some(parent), content: ItemContent::ToolCall { name, .. }, .. })
                if id == "toolu_child:toolu_list" && parent == "toolu_child" && name == "ListAgents"
        )));
        assert!(
            matches!(
                events.last(),
                Some(AgentEvent::ItemCompleted(ThreadItem { id, parent_item_id: Some(parent), content: ItemContent::Subagent { agent_type, status: ItemStatus::Completed, summary: Some(summary), .. }, .. }))
                    if id == "toolu_nested" && parent == "toolu_child" && agent_type == "Explore" && summary == "Tests are green."
            ),
            "{events:?}"
        );
        assert!(mapper.take_pending_subagent_terminals().is_empty());
    }

    #[test]
    fn only_spawning_tools_are_agent_tools() {
        for name in ["agent", "task"] {
            assert!(is_agent_tool(name), "{name}");
        }
        for name in ["listagents", "sendmessage", "taskcreate", "subagent_run"] {
            assert!(!is_agent_tool(name), "{name}");
        }
        let mut mapper = Mapper::new();
        let events = feed(
            &mut mapper,
            r#"{"type":"assistant","message":{"id":"msg-list","content":[{"type":"tool_use","id":"toolu_list","name":"ListAgents","input":{}}]}}"#,
        );
        assert!(matches!(
            events.as_slice(),
            [AgentEvent::ItemStarted(ThreadItem { content: ItemContent::ToolCall { name, .. }, .. })] if name == "ListAgents"
        ));
    }

    /// The tail keeps the transcript discovery found; the notification's
    /// `output_file` only seeds a reader when discovery found nothing.
    #[test]
    fn subagent_tail_prefer_path_does_not_replay_a_discovered_transcript() {
        let root = std::env::temp_dir().join(format!(
            "tcode-subagent-tail-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let subagents = root.join("projects/session-tail/subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        let transcript = "{\"type\":\"user\",\"message\":{\"content\":\"go\"}}\n{\"type\":\"assistant\",\"message\":{\"id\":\"m1\",\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}}\n";
        std::fs::write(
            subagents.join("agent-x.meta.json"),
            r#"{"toolUseId":"toolu_tail"}"#,
        )
        .unwrap();
        std::fs::write(subagents.join("agent-x.jsonl"), transcript).unwrap();
        let output_file = root.join("task.output");
        std::fs::write(&output_file, transcript).unwrap();

        let run = |claude_dir: Option<PathBuf>| {
            let (control_tx, control_rx) = smol::channel::unbounded();
            let (notice_tx, notice_rx) = smol::channel::unbounded();
            smol::spawn(run_subagent_tail(
                "toolu_tail".into(),
                "task-tail".into(),
                "session-tail".into(),
                claude_dir,
                control_rx,
                notice_tx,
            ))
            .detach();
            (control_tx, notice_rx)
        };
        smol::block_on(async {
            let (control, notices) = run(Some(root.clone()));
            let first = notices.recv().await.unwrap();
            assert_eq!(first.events.len(), 2);
            assert!(!first.stopped);
            control
                .send(TailControl::PreferPath(output_file.clone()))
                .await
                .unwrap();
            control.send(TailControl::Stop).await.unwrap();
            let mut after = Vec::new();
            loop {
                let notice = notices.recv().await.unwrap();
                after.extend(notice.events);
                if notice.stopped {
                    break;
                }
            }
            assert!(
                after.is_empty(),
                "preferred path must not be re-read: {after:?}"
            );

            let (control, notices) = run(None);
            control
                .send(TailControl::PreferPath(output_file.clone()))
                .await
                .unwrap();
            control.send(TailControl::Stop).await.unwrap();
            let mut fallback = Vec::new();
            loop {
                let notice = notices.recv().await.unwrap();
                fallback.extend(notice.events);
                if notice.stopped {
                    break;
                }
            }
            assert_eq!(
                fallback.len(),
                2,
                "undiscovered tail reads the notified file"
            );
        });
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn turn_acceptance_requires_a_flushed_write_and_steer_failures_remain_fatal() {
        smol::block_on(async {
            use std::io;
            use std::pin::Pin;
            use std::task::{Context, Poll};

            #[derive(Clone, Copy, PartialEq, Eq)]
            enum FailurePoint {
                Write,
                Flush,
            }

            #[derive(Default)]
            struct DeterministicWriter {
                failure: Option<FailurePoint>,
                bytes: Vec<u8>,
            }

            impl AsyncWrite for DeterministicWriter {
                fn poll_write(
                    mut self: Pin<&mut Self>,
                    _cx: &mut Context<'_>,
                    bytes: &[u8],
                ) -> Poll<io::Result<usize>> {
                    if self.failure == Some(FailurePoint::Write) {
                        Poll::Ready(Err(io::Error::other("deterministic write failure")))
                    } else {
                        self.bytes.extend_from_slice(bytes);
                        Poll::Ready(Ok(bytes.len()))
                    }
                }

                fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                    if self.failure == Some(FailurePoint::Flush) {
                        Poll::Ready(Err(io::Error::other("deterministic flush failure")))
                    } else {
                        Poll::Ready(Ok(()))
                    }
                }

                fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                    Poll::Ready(Ok(()))
                }
            }

            let message = user_message("deliver", &[]);
            let (event_tx, event_rx) = smol::channel::unbounded();
            let mut writer = DeterministicWriter::default();
            write_turn_message(&mut writer, &message, 42, &event_tx)
                .await
                .unwrap();
            assert!(matches!(
                event_rx.recv().await.unwrap(),
                AgentEvent::TurnAccepted { delivery_id: 42 }
            ));
            assert_eq!(writer.bytes.last(), Some(&b'\n'));

            let (event_tx, event_rx) = smol::channel::unbounded();
            let mut writer = DeterministicWriter {
                failure: Some(FailurePoint::Flush),
                ..Default::default()
            };
            assert!(
                write_turn_message(&mut writer, &message, 43, &event_tx)
                    .await
                    .is_err()
            );
            assert!(event_rx.try_recv().is_err(), "failed flush must not ack");

            let (event_tx, event_rx) = smol::channel::unbounded();
            let mut writer = DeterministicWriter::default();
            let mut mapper = Mapper::new();
            let message = user_message("redirect", &[]);
            write_steering_message(
                &mut writer,
                &message,
                "steer-ok".into(),
                &mut mapper,
                &event_tx,
            )
            .await;
            assert!(event_rx.try_recv().is_err());
            assert_eq!(mapper.pending_steers, VecDeque::from(["steer-ok".into()]));
            assert_eq!(writer.bytes.last(), Some(&b'\n'));

            let (event_tx, event_rx) = smol::channel::unbounded();
            let mut mapper = Mapper::new();
            let mut writer = DeterministicWriter {
                failure: Some(FailurePoint::Write),
                ..Default::default()
            };
            write_steering_message(
                &mut writer,
                &message,
                "steer-write-failed".into(),
                &mut mapper,
                &event_tx,
            )
            .await;
            assert!(matches!(
                event_rx.recv().await.unwrap(),
                AgentEvent::Error { ref message, fatal: true }
                    if message == "failed to write steering message to provider stdin"
            ));
            assert!(
                event_rx.try_recv().is_err(),
                "stdin write failures must not accept a steer"
            );

            let (event_tx, event_rx) = smol::channel::unbounded();
            let mut mapper = Mapper::new();
            let mut writer = DeterministicWriter {
                failure: Some(FailurePoint::Flush),
                ..Default::default()
            };
            write_steering_message(
                &mut writer,
                &message,
                "steer-flush-failed".into(),
                &mut mapper,
                &event_tx,
            )
            .await;
            assert!(matches!(
                event_rx.recv().await.unwrap(),
                AgentEvent::Error { ref message, fatal: true }
                    if message == "failed to write steering message to provider stdin"
            ));
            assert!(
                event_rx.try_recv().is_err(),
                "stdin flush failures must not accept a steer"
            );
        });
    }

    fn accepted_request_ids(events: &[AgentEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::SteerAccepted { request_id } => Some(request_id.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn steer_acceptance_uses_request_boundaries_with_a_legacy_assistant_fallback() {
        let requesting = r#"{"type":"system","subtype":"status","status":"requesting","uuid":"checkpoint","session_id":"session"}"#;
        let assistant = r#"{"type":"assistant","message":{"model":"claude-opus-4-8","id":"msg","content":[{"type":"text","text":"response"}]},"session_id":"session"}"#;
        for current_cli in [false, true] {
            let mut mapper = Mapper::new();
            if current_cli {
                assert!(feed(&mut mapper, requesting).is_empty());
            }
            mapper
                .pending_steers
                .extend(["first".into(), "second".into()]);
            let result = feed(
                &mut mapper,
                r#"{"type":"result","subtype":"success","is_error":false,"session_id":"session"}"#,
            );
            assert!(accepted_request_ids(&result).is_empty());
            assert_eq!(mapper.pending_steers.len(), 2);
            let events = feed(&mut mapper, assistant);
            if current_cli {
                assert!(accepted_request_ids(&events).is_empty());
                assert_eq!(mapper.pending_steers.len(), 2);
                assert_eq!(
                    accepted_request_ids(&feed(&mut mapper, requesting)),
                    ["first", "second"]
                );
            } else {
                assert_eq!(accepted_request_ids(&events), ["first", "second"]);
            }
            assert!(mapper.pending_steers.is_empty());
            assert!(accepted_request_ids(&feed(&mut mapper, requesting)).is_empty());
            assert!(accepted_request_ids(&feed(&mut mapper, assistant)).is_empty());
        }
    }

    #[test]
    fn assistant_served_model_is_emitted_once_per_change() {
        let mut mapper = Mapper::new();
        let line = r#"{"type":"assistant","message":{"model":"claude-sonnet-4","id":"msg-1","content":[]}}"#;
        let first = feed(&mut mapper, line);
        let repeat = feed(&mut mapper, line);
        assert!(matches!(
            first.as_slice(),
            [AgentEvent::ServedModel { model, reason: None }] if model == "claude-sonnet-4"
        ));
        assert!(repeat.is_empty());
        let changed = feed(
            &mut mapper,
            r#"{"type":"assistant","message":{"model":"claude-opus-4-8","id":"msg-1","content":[]}}"#,
        );
        assert!(
            matches!(changed.as_slice(), [AgentEvent::ServedModel { model, reason: None }] if model == "claude-opus-4-8")
        );
    }

    #[test]
    fn refusal_stop_reason_warns_once_across_delta_and_assistant() {
        let mut mapper = Mapper::new();
        let delta = feed(
            &mut mapper,
            r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"refusal"}}}"#,
        );
        let assistant = feed(
            &mut mapper,
            r#"{"type":"assistant","message":{"id":"msg-1","stop_reason":"refusal","content":[]}}"#,
        );
        assert!(matches!(
            delta.as_slice(),
            [AgentEvent::Warning { message }]
                if message == "Request declined by safety classifiers (stop_reason: refusal)"
        ));
        assert!(assistant.is_empty());
    }

    #[test]
    fn structured_patch_renders_unified_diff() {
        let patch = json!([{
            "oldStart": 3,
            "oldLines": 2,
            "newStart": 3,
            "newLines": 2,
            "lines": [" keep", "-old", "+new"]
        }]);
        assert_eq!(
            render_structured_patch(&patch).as_deref(),
            Some("@@ -3,2 +3,2 @@\n keep\n-old\n+new")
        );
    }
}

fn rewind_files(result: &Value) -> RewindFiles {
    RewindFiles {
        files: result
            .get("filesChanged")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(RewindFile::path)
            .collect(),
        insertions: result.get("insertions").and_then(Value::as_u64),
        deletions: result.get("deletions").and_then(Value::as_u64),
        skipped_links: result
            .get("skippedLinks")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
    }
}

/// Why `rewind_files` could not restore, from the words Claude Code answers
/// with: it gives no reason code for files.
fn files_failure(error: Option<String>) -> RewindFailure {
    let reason = match error.as_deref() {
        Some(error) if error.contains("No file checkpoint") => {
            RewindFailureReason::NothingToRestore
        }
        Some(error) if error.contains("not enabled") => RewindFailureReason::Unsupported,
        _ => RewindFailureReason::Other,
    };
    RewindFailure::new(reason, error)
}

/// `rewind_conversation`'s refusal `reason`, in the canonical terms.
fn conversation_refusal(reason: Option<&str>) -> RewindFailureReason {
    match reason {
        Some("commands_queued" | "prompt_pending") => RewindFailureReason::Busy,
        Some("turn_running") => RewindFailureReason::TurnRunning,
        Some("target_not_found") => RewindFailureReason::TargetNotFound,
        Some("stale_target" | "unseen_later_turn" | "state_changed") => {
            RewindFailureReason::ConversationChanged
        }
        Some(
            "poll_tool_result_target"
            | "target_splits_tool_call"
            | "delivered_poll_events_in_range",
        ) => RewindFailureReason::CannotCutHere,
        Some("persist_failed") => RewindFailureReason::SaveFailed,
        Some("usage_limited") => RewindFailureReason::UsageLimited,
        Some("expired") => RewindFailureReason::Expired,
        _ => RewindFailureReason::Other,
    }
}
