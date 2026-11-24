//! Headless end-to-end probe for provider clients.
//!
//! Catalog mode: `probe --list-models <codex|claude|pi|opencode|cursor|grok> [--binary <path>]`.
//! Plugin mode: `probe plugins <provider> [--home <dir>] [--project <dir>] [<op> …]`
//! lists the native plugin catalog after running the optional operation:
//! `install|update <id> <scope> [--accept <sha256>]`, `uninstall|enable|disable <id> <scope>`,
//! `add-marketplace <source>` or `remove-marketplace <name>`. `--home` isolates
//! the provider's native state (`CODEX_HOME` for Codex, `GROK_HOME` for Grok;
//! for Claude Code `HOME` and `CLAUDE_CONFIG_DIR`).
//! Turn mode: `probe <provider> <prompt> [cwd] [acp-command args…] [flags]`.
//! Flags are `--binary <path>`, `--model <id>`, `--option <id>=<value>`, `--effort <value>`,
//! `--resume <cursor-json>`, `--fork`, `--leave-questions` (user-input requests
//! stay unanswered), `--mcp <name> <url> <token>` (an HTTP MCP server registered
//! as tcode registers its own), `--linger <seconds>` (stay open until no turn has
//! run for that long, to see turns the agent starts itself), `--follow-up
//! <seconds> <prompt>` (send another turn that long after the first completes,
//! running turn or not), `--interrupt-after <seconds>`, `--steer <message>`, and
//! `--image <path>`. Only one of the last three may be used.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use agent::{
    AcpAgent, AcpLaunch, AgentEvent, ApprovalDecision, Attachment, ItemContent, LaunchEnv,
    McpRegistration, OptionSelection, PluginContext, PluginOp, PluginScope, ProviderKind,
    ResumeCursor, SessionCommand, SessionOptions, TurnOptions, TurnStatus, list_models,
    list_plugins, run_plugin_op, start_session,
};
use base64::Engine as _;

const STEER_DELAY: Duration = Duration::from_secs(10);
const PHANTOM_TURN_GRACE: Duration = Duration::from_secs(5);

#[derive(Default)]
enum ProbeMode {
    #[default]
    Standard,
    Interrupt(Duration),
    Steer(String),
    Image(Attachment),
    /// The prompt is the objective of a goal rather than a turn.
    Goal,
}

fn usage() -> ! {
    eprintln!(
        "usage: probe <codex|claude|pi|opencode|cursor|grok|acp> <prompt> [cwd] \
         [--option <id>=<value>] [acp-command args…] [flags]"
    );
    eprintln!(
        "       probe --list-models <codex|claude|pi|opencode|cursor|grok> [--binary <path>]"
    );
    eprintln!("       probe plugins <provider> [--home <dir>] [--project <dir>] [<op> …]");
    std::process::exit(2);
}

fn parse_scope(arg: Option<String>) -> PluginScope {
    match arg.as_deref() {
        Some("user") => PluginScope::User,
        Some("project") => PluginScope::Project,
        Some("local") => PluginScope::Local,
        Some("managed") => PluginScope::Managed,
        _ => usage(),
    }
}

fn plugins(mut args: impl Iterator<Item = String>) -> i32 {
    let provider = parse_provider(args.next().as_deref());
    let mut home = None;
    let mut project = None;
    let mut accept = None;
    let mut positional = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--home" => home = args.next().map(PathBuf::from),
            "--project" => project = args.next().map(PathBuf::from),
            "--accept" => accept = args.next(),
            _ => positional.push(arg),
        }
    }
    let mut env = Vec::new();
    if let (ProviderKind::ClaudeCode, Some(home)) = (provider, &home) {
        env.push((
            "CLAUDE_CONFIG_DIR".to_string(),
            home.join(".claude").display().to_string(),
        ));
    }
    let context = PluginContext {
        binary_path: None,
        launch_env: LaunchEnv {
            env,
            home: home.clone(),
        },
        cwd: project
            .clone()
            .or(home)
            .unwrap_or_else(|| std::env::current_dir().unwrap()),
        project: project.is_some(),
    };
    let mut positional = positional.into_iter();
    let op = positional.next().map(|verb| {
        let mut id = || positional.next().unwrap_or_else(|| usage());
        match verb.as_str() {
            "install" => PluginOp::Install {
                id: id(),
                scope: parse_scope(positional.next()),
                accept_command: accept.clone(),
            },
            "update" => PluginOp::Update {
                id: id(),
                scope: parse_scope(positional.next()),
                accept_command: accept.clone(),
            },
            "uninstall" => PluginOp::Uninstall {
                id: id(),
                scope: parse_scope(positional.next()),
            },
            "enable" | "disable" => PluginOp::SetEnabled {
                id: id(),
                scope: parse_scope(positional.next()),
                enabled: verb == "enable",
            },
            "add-marketplace" => PluginOp::AddMarketplace { source: id() },
            "remove-marketplace" => PluginOp::RemoveMarketplace { name: id() },
            _ => usage(),
        }
    });
    smol::block_on(async move {
        if let Some(op) = op {
            eprintln!("probe: {op:?} in {}", context.cwd.display());
            match run_plugin_op(provider, &context, &op).await {
                Ok(outcome) => println!("outcome: {outcome:#?}"),
                Err(error) => println!("outcome: error: {error}"),
            }
        }
        match list_plugins(provider, &context).await {
            Ok(listing) => {
                let listing = serde_json::json!({
                    "marketplaces": listing.marketplaces,
                    "marketplace_actions": listing.marketplace_actions,
                    "entries": listing.entries,
                });
                println!("{}", serde_json::to_string_pretty(&listing).unwrap());
                0
            }
            Err(error) => {
                eprintln!("list_plugins failed: {error}");
                1
            }
        }
    })
}

fn parse_provider(arg: Option<&str>) -> ProviderKind {
    match arg {
        Some("codex") => ProviderKind::Codex,
        Some("claude") => ProviderKind::ClaudeCode,
        Some("pi") => ProviderKind::Pi,
        Some("opencode") => ProviderKind::OpenCode,
        Some("cursor") => ProviderKind::Cursor,
        Some("grok") => ProviderKind::Grok,
        Some("acp") => ProviderKind::Acp,
        _ => usage(),
    }
}

fn set_mode(mode: &mut ProbeMode, replacement: ProbeMode) {
    if !matches!(mode, ProbeMode::Standard) {
        eprintln!("use only one of --interrupt-after, --steer, --image and --goal");
        std::process::exit(2);
    }
    *mode = replacement;
}

fn image_attachment(path: PathBuf) -> Attachment {
    let bytes = std::fs::read(&path).unwrap_or_else(|error| {
        eprintln!("failed to read {}: {error}", path.display());
        std::process::exit(2);
    });
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    let media_type = match extension.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "png" => "image/png",
        other => {
            eprintln!("probe: unknown image extension {other:?}; defaulting to image/png");
            "image/png"
        }
    };
    eprintln!(
        "probe: image={} ({} bytes, {media_type})",
        path.display(),
        bytes.len()
    );
    Attachment {
        media_type: media_type.into(),
        data_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        source_path: None,
    }
}

fn main() {
    env_logger::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("plugins") {
        std::process::exit(plugins(args.into_iter().skip(1)));
    }
    if args.first().map(String::as_str) == Some("--list-models") {
        let provider = parse_provider(args.get(1).map(String::as_str));
        let binary = match args.get(2).map(String::as_str) {
            Some("--binary") => Some(args.get(3).map(PathBuf::from).unwrap_or_else(|| usage())),
            Some(_) => usage(),
            None => None,
        };
        let exit_code = smol::block_on(async move {
            match list_models(provider, binary, Default::default(), Default::default()).await {
                Ok(models) => {
                    println!("{}", serde_json::to_string_pretty(&models).unwrap());
                    0
                }
                Err(error) => {
                    eprintln!("list_models failed: {error}");
                    1
                }
            }
        });
        std::process::exit(exit_code);
    }

    let mut binary = None;
    let mut model = None;
    let mut effort = None;
    let mut option_selections = Vec::new();
    let mut resume = None;
    let mut fork = false;
    let mut leave_questions = false;
    let mut mcp_servers = Vec::new();
    let mut continuation = Continuation::default();
    let mut probe_mode = ProbeMode::Standard;
    let mut positional = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--binary" => binary = Some(args.next().map(PathBuf::from).unwrap_or_else(|| usage())),
            "--model" => model = Some(args.next().unwrap_or_else(|| usage())),
            "--effort" => effort = Some(args.next().unwrap_or_else(|| usage())),
            "--option" => {
                let option = args.next().unwrap_or_else(|| usage());
                let (id, value) = option.split_once('=').unwrap_or_else(|| usage());
                option_selections.push(OptionSelection {
                    id: id.into(),
                    value: serde_json::from_str(value).unwrap_or_else(|_| serde_json::json!(value)),
                });
            }
            "--resume" => {
                let cursor = args.next().unwrap_or_else(|| usage());
                resume = Some(ResumeCursor(serde_json::from_str(&cursor).unwrap_or_else(
                    |error| {
                        eprintln!("--resume takes the JSON of a resume cursor: {error}");
                        std::process::exit(2);
                    },
                )));
            }
            "--fork" => fork = true,
            "--goal" => set_mode(&mut probe_mode, ProbeMode::Goal),
            "--leave-questions" => leave_questions = true,
            "--linger" => {
                let seconds = args.next().and_then(|value| value.parse().ok());
                continuation.linger = Some(Duration::from_secs(seconds.unwrap_or_else(|| usage())));
            }
            "--follow-up" => {
                let seconds = args.next().and_then(|value| value.parse().ok());
                let seconds = seconds.unwrap_or_else(|| usage());
                let prompt = args.next().unwrap_or_else(|| usage());
                continuation.follow_up = Some((Duration::from_secs(seconds), prompt));
            }
            "--mcp" => {
                let mut next = || args.next().unwrap_or_else(|| usage());
                mcp_servers.push(McpRegistration {
                    name: next(),
                    url: next(),
                    bearer_token: next(),
                });
            }
            "--interrupt-after" => {
                let seconds = args
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or_else(|| {
                        eprintln!("--interrupt-after requires a number of seconds");
                        std::process::exit(2);
                    });
                set_mode(
                    &mut probe_mode,
                    ProbeMode::Interrupt(Duration::from_secs(seconds)),
                );
            }
            "--steer" => {
                let message = args.next().unwrap_or_else(|| usage());
                set_mode(&mut probe_mode, ProbeMode::Steer(message));
            }
            "--image" => {
                let path = args.next().map(PathBuf::from).unwrap_or_else(|| usage());
                set_mode(&mut probe_mode, ProbeMode::Image(image_attachment(path)));
            }
            _ => positional.push(arg),
        }
    }

    let mut positional = positional.into_iter();
    let provider = parse_provider(positional.next().as_deref());
    let prompt = positional.next().unwrap_or_else(|| usage());
    let cwd = positional.next().map(PathBuf::from).unwrap_or_else(|| {
        if matches!(probe_mode, ProbeMode::Steer(_) | ProbeMode::Image(_)) {
            std::env::temp_dir()
        } else {
            std::env::current_dir().unwrap()
        }
    });
    let remaining: Vec<String> = positional.collect();
    let acp = if provider == ProviderKind::Acp {
        let mut launch = remaining.into_iter();
        let command = launch.next().unwrap_or_else(|| {
            eprintln!("the acp provider requires a command and optional arguments");
            usage()
        });
        Some(AcpAgent {
            id: "probe".into(),
            name: command.clone(),
            launch: AcpLaunch::Custom {
                command,
                args: launch.collect(),
                env: Vec::new(),
            },
        })
    } else {
        if !remaining.is_empty() {
            usage();
        }
        None
    };
    let exit_code = smol::block_on(run_probe(
        provider,
        prompt,
        cwd,
        option_selections,
        effort,
        probe_mode,
        acp,
        mcp_servers,
        continuation,
        Resumption {
            resume,
            fork,
            leave_questions,
        },
        binary,
        model,
    ));
    std::process::exit(exit_code);
}

struct Resumption {
    resume: Option<ResumeCursor>,
    fork: bool,
    leave_questions: bool,
}

#[derive(Default)]
struct Continuation {
    linger: Option<Duration>,
    follow_up: Option<(Duration, String)>,
}

#[allow(clippy::too_many_arguments)]
async fn run_probe(
    provider: ProviderKind,
    prompt: String,
    cwd: PathBuf,
    mut option_selections: Vec<OptionSelection>,
    effort: Option<String>,
    probe_mode: ProbeMode,
    acp: Option<AcpAgent>,
    mcp_servers: Vec<McpRegistration>,
    continuation: Continuation,
    resumption: Resumption,
    binary_path: Option<PathBuf>,
    model: Option<String>,
) -> i32 {
    option_selections.extend(effort.iter().map(|value| OptionSelection {
        id: "reasoningEffort".into(),
        value: serde_json::Value::String(value.clone()),
    }));
    let model = model.or_else(|| match (provider, effort.is_some()) {
        (ProviderKind::ClaudeCode, true) => Some("claude-opus-4-8".to_string()),
        _ => None,
    });
    let opts = SessionOptions {
        cwd,
        model,
        resume: resumption.resume,
        fork: resumption.fork,
        binary_path,
        option_selections,
        mcp_servers,
        launch_env: Default::default(),
        extra_args: Vec::new(),
        acp,
        instructions: None,
    };
    let handle = match start_session(provider, opts).await {
        Ok(handle) => handle,
        Err(error) => {
            if provider == ProviderKind::Acp {
                println!("START FAILED: {error}");
            } else {
                eprintln!("failed to start session: {error}");
            }
            return 1;
        }
    };
    let attachments = match &probe_mode {
        ProbeMode::Image(attachment) => vec![attachment.clone()],
        _ => Vec::new(),
    };
    let first = if matches!(probe_mode, ProbeMode::Goal) {
        SessionCommand::SetGoal {
            objective: Some(prompt),
            status: None,
        }
    } else {
        SessionCommand::SendTurn {
            delivery_id: 0,
            text: prompt,
            options: Some(TurnOptions { effort }),
            attachments,
        }
    };
    handle
        .commands
        .send(first)
        .await
        .expect("session command channel closed before first turn");

    match &probe_mode {
        ProbeMode::Interrupt(delay) => {
            let commands = handle.commands.clone();
            let delay = *delay;
            smol::spawn(async move {
                smol::Timer::after(delay).await;
                eprintln!("--- sending Interrupt ---");
                commands.send(SessionCommand::Interrupt).await.ok();
                smol::Timer::after(Duration::from_secs(30)).await;
                eprintln!("--- interrupt timed out, forcing shutdown ---");
                commands.send(SessionCommand::Shutdown).await.ok();
            })
            .detach();
        }
        ProbeMode::Steer(message) => {
            let commands = handle.commands.clone();
            let message = message.clone();
            smol::spawn(async move {
                smol::Timer::after(STEER_DELAY).await;
                eprintln!("probe: STEERING (mid-turn) -> {message:?}");
                commands
                    .send(SessionCommand::Steer {
                        request_id: "probe-steer-1".into(),
                        text: message,
                        attachments: Vec::new(),
                    })
                    .await
                    .ok();
            })
            .detach();
        }
        _ => {}
    }

    let mut assistant = String::new();
    let mut turns_started = 0;
    let mut turns_completed = 0;
    let mut first_status = None;
    let mut steer_accepted = false;
    let lingering = continuation
        .linger
        .or(continuation.follow_up.as_ref().map(|_| PHANTOM_TURN_GRACE));
    let mut follow_up = continuation.follow_up;
    let mut open_until = None;
    let mut running = false;
    let mut closing = false;
    loop {
        let quiet = match lingering {
            Some(linger) if turns_completed > 0 && !running && !closing => Some(linger),
            _ if matches!(probe_mode, ProbeMode::Steer(_)) && turns_completed > 0 => {
                Some(PHANTOM_TURN_GRACE)
            }
            _ => None,
        };
        let event = match quiet {
            Some(quiet) => {
                let event = smol::future::or(handle.events.recv(), async {
                    smol::Timer::after(quiet).await;
                    Err(smol::channel::RecvError)
                })
                .await
                .ok();
                if event.is_none() {
                    if open_until.is_none_or(|until| Instant::now() >= until) {
                        closing = true;
                        handle.commands.send(SessionCommand::Shutdown).await.ok();
                    }
                    continue;
                }
                event
            }
            None => handle.events.recv().await.ok(),
        };
        let Some(event) = event else { break };
        match &event {
            AgentEvent::TurnStarted { .. } => running = true,
            AgentEvent::TurnCompleted { .. } => running = false,
            _ => {}
        }

        if provider == ProviderKind::Acp {
            match &event {
                AgentEvent::Delta { kind, text, .. } => println!("DELTA {kind:?}: {text:?}"),
                AgentEvent::ApprovalRequested(request) => {
                    println!("APPROVAL {:?} options={:?}", request.kind, request.options);
                }
                AgentEvent::TurnCompleted { status, usage, .. } => {
                    println!("TURN {status:?} usage={usage:?}");
                }
                other => println!("{other:?}"),
            }
        } else if !matches!(
            (&probe_mode, &event),
            (ProbeMode::Interrupt(_), AgentEvent::Delta { .. })
        ) && !matches!(probe_mode, ProbeMode::Image(_) | ProbeMode::Steer(_))
        {
            println!("{}", serde_json::to_string(&event).unwrap());
        }
        match &event {
            AgentEvent::ApprovalRequested(request) => {
                handle
                    .commands
                    .send(SessionCommand::RespondApproval {
                        request_id: request.id.clone(),
                        decision: request
                            .options
                            .iter()
                            .find(|option| option.kind == agent::ApprovalOptionKind::AllowOnce)
                            .map(|option| ApprovalDecision::Option(option.id.clone()))
                            .unwrap_or(ApprovalDecision::Cancel),
                    })
                    .await
                    .ok();
            }
            AgentEvent::UserInputRequested { questions, .. } if resumption.leave_questions => {
                eprintln!("probe: leaving {} question(s) unanswered", questions.len());
            }
            AgentEvent::UserInputRequested {
                request_id,
                questions,
                ..
            } => {
                let answers = questions
                    .iter()
                    .map(|question| {
                        let answer = question
                            .options
                            .first()
                            .map(|option| option.label.clone())
                            .unwrap_or_default();
                        eprintln!(
                            "probe: user-input {:?} header={:?} options={:?} -> answering {:?}",
                            question.question,
                            question.header,
                            question
                                .options
                                .iter()
                                .map(|option| &option.label)
                                .collect::<Vec<_>>(),
                            answer
                        );
                        (question.id.clone(), serde_json::Value::String(answer))
                    })
                    .collect();
                handle
                    .commands
                    .send(SessionCommand::RespondUserInput {
                        request_id: request_id.clone(),
                        answers,
                    })
                    .await
                    .ok();
            }
            AgentEvent::ItemCompleted(item) => {
                if let ItemContent::AssistantMessage { text } = &item.content {
                    assistant.push_str(text);
                    assistant.push('\n');
                }
            }
            AgentEvent::ProviderCommands { commands }
                if matches!(probe_mode, ProbeMode::Image(_)) =>
            {
                eprintln!("probe: provider reported {} command(s)", commands.len());
            }
            AgentEvent::Warning { message } if matches!(probe_mode, ProbeMode::Steer(_)) => {
                eprintln!("probe: WARNING: {message}");
            }
            AgentEvent::Error { message, fatal }
                if matches!(probe_mode, ProbeMode::Image(_) | ProbeMode::Steer(_)) =>
            {
                eprintln!("probe: provider error (fatal={fatal}): {message}");
                if *fatal {
                    handle.commands.send(SessionCommand::Shutdown).await.ok();
                }
            }
            AgentEvent::TurnStarted { turn_id } => {
                turns_started += 1;
                if matches!(probe_mode, ProbeMode::Steer(_)) {
                    eprintln!("probe: TurnStarted {turn_id} (#{turns_started})");
                }
            }
            AgentEvent::SteerAccepted { request_id }
                if request_id == "probe-steer-1" && turns_completed == 0 =>
            {
                steer_accepted = true;
                eprintln!("probe: SteerAccepted {request_id} before TurnCompleted");
            }
            AgentEvent::TurnCompleted {
                status, turn_id, ..
            } => {
                turns_completed += 1;
                first_status.get_or_insert(*status);
                if matches!(probe_mode, ProbeMode::Steer(_)) {
                    eprintln!(
                        "probe: TurnCompleted {turn_id} status={status:?} (#{turns_completed})"
                    );
                }
                if let Some((delay, text)) = follow_up.take() {
                    open_until = Some(Instant::now() + delay + Duration::from_secs(1));
                    let commands = handle.commands.clone();
                    smol::spawn(async move {
                        smol::Timer::after(delay).await;
                        eprintln!("probe: FOLLOW-UP -> {text:?}");
                        commands
                            .send(SessionCommand::SendTurn {
                                delivery_id: 1,
                                text,
                                options: None,
                                attachments: Vec::new(),
                            })
                            .await
                            .ok();
                    })
                    .detach();
                }
                if !matches!(probe_mode, ProbeMode::Steer(_)) && lingering.is_none() {
                    handle.commands.send(SessionCommand::Shutdown).await.ok();
                }
            }
            AgentEvent::Error { fatal: true, .. } => {
                handle.commands.send(SessionCommand::Shutdown).await.ok();
            }
            AgentEvent::SessionClosed { .. } => break,
            _ => {}
        }
    }
    handle.commands.send(SessionCommand::Shutdown).await.ok();

    match probe_mode {
        ProbeMode::Standard if provider == ProviderKind::Acp => {
            println!("session closed");
            i32::from(first_status != Some(TurnStatus::Completed))
        }
        ProbeMode::Standard | ProbeMode::Goal => i32::from(first_status.is_none()),
        ProbeMode::Interrupt(_) => match first_status {
            Some(TurnStatus::Interrupted) => {
                eprintln!("OK: turn was interrupted");
                0
            }
            other => {
                eprintln!("FAIL: expected Interrupted, got {other:?}");
                1
            }
        },
        ProbeMode::Image(_) => {
            println!("ASSISTANT: {}", assistant.trim());
            i32::from(first_status != Some(TurnStatus::Completed))
        }
        ProbeMode::Steer(message) => {
            let marker = message
                .split_whitespace()
                .next_back()
                .unwrap_or_default()
                .trim_matches(|character: char| !character.is_alphanumeric())
                .to_uppercase();
            let steered = !marker.is_empty() && assistant.to_uppercase().contains(&marker);
            let clean_accounting = turns_started == 1 && turns_completed == 1;
            println!("--- transcript ---\n{}", assistant.trim());
            println!("--- steering marker {marker} present: {steered} ---");
            println!("--- steer acceptance before completion observed: {steer_accepted} ---");
            println!(
                "--- turn accounting: TurnStarted={turns_started} \
                 TurnCompleted={turns_completed} (both must be 1) ---"
            );
            i32::from(
                first_status != Some(TurnStatus::Completed)
                    || !steered
                    || !clean_accounting
                    || !steer_accepted,
            )
        }
    }
}
