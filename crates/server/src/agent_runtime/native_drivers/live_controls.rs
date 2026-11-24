//! Opt-in acceptance against installed native CLIs and source Ollama/Ark accounts.
use super::*;

async fn next(handle: &SessionHandle) -> AgentEvent {
    tokio::time::timeout(Duration::from_secs(120), handle.events.recv())
        .await
        .expect("native event timed out")
        .expect("native event stream closed")
}
async fn send(handle: &SessionHandle, delivery_id: u64, text: &str) {
    handle
        .commands
        .send(SessionCommand::SendTurn {
            delivery_id,
            text: text.into(),
            options: None,
            attachments: Vec::new(),
        })
        .await
        .unwrap();
}
async fn controls(handle: &SessionHandle, native_start_event: bool) {
    send(handle, 1, "Execute `sleep 30; printf MUTEKI_UNEXPECTED_FINISH` with your shell tool. This is an isolated runtime interruption check.").await;
    let mut interrupted = false;
    loop {
        match next(handle).await {
            AgentEvent::ItemStarted(ThreadItem {
                content: ItemContent::ToolCall { .. },
                ..
            }) if !interrupted => {
                handle
                    .commands
                    .send(SessionCommand::Interrupt)
                    .await
                    .unwrap();
                interrupted = true;
            }
            AgentEvent::TurnCompleted { status, .. } => {
                assert!(interrupted, "model did not start its shell tool");
                assert_eq!(status, TurnStatus::Interrupted);
                break;
            }
            AgentEvent::TurnFailed { code, message } => panic!("{code}: {message}"),
            _ => {}
        }
    }
    eprintln!("native interruption observed");
    send(handle, 2, "Execute `sleep 3; printf MUTEKI_STEER_READY` with your shell tool. Then report the output and comply with any new operator message received during execution.").await;
    let mut steered = false;
    let mut accepted = false;
    let mut reply = String::new();
    loop {
        match next(handle).await {
            AgentEvent::ItemStarted(ThreadItem {
                content: ItemContent::ToolCall { .. },
                ..
            }) if !steered => {
                handle
                    .commands
                    .send(SessionCommand::Steer {
                        request_id: "live-native-steer".into(),
                        text: "Also include exactly MUTEKI_STEER_ACCEPTED in your final reply."
                            .into(),
                        attachments: Vec::new(),
                    })
                    .await
                    .unwrap();
                steered = true;
            }
            AgentEvent::SteerAccepted { request_id } if request_id == "live-native-steer" => {
                accepted = true
            }
            AgentEvent::SteerRejected { request_id } => panic!("steer rejected: {request_id}"),
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::AssistantMessage { text },
                ..
            }) => reply.push_str(&text),
            AgentEvent::TurnCompleted { status, .. } => {
                assert_eq!(status, TurnStatus::Completed);
                break;
            }
            AgentEvent::TurnFailed { code, message } => panic!("{code}: {message}"),
            AgentEvent::Error { message, .. } => panic!("native control failed: {message}"),
            _ => {}
        }
    }
    assert!(
        accepted && reply.contains("MUTEKI_STEER_ACCEPTED"),
        "steer accepted={accepted}; reply={reply}"
    );
    eprintln!("native steering acknowledgement and actual model consumption observed");
    handle.commands.send(SessionCommand::Compact).await.unwrap();
    let mut started = false;
    loop {
        match next(handle).await {
            AgentEvent::ContextCompacted(info) if info.in_progress => started = true,
            AgentEvent::ContextCompacted(info) if !info.in_progress => {
                assert!(!native_start_event || started, "no native compaction start");
                assert!(
                    info.pre_tokens.is_some_and(|tokens| tokens > 0),
                    "no native compaction result"
                );
                break;
            }
            AgentEvent::Error { message, .. } => panic!("native compaction failed: {message}"),
            AgentEvent::Warning { message } => panic!("native compaction warning: {message}"),
            _ => {}
        }
    }
    eprintln!("native compaction completion observed");
}

async fn kimi(access: &str) -> (SessionHandle, tempfile::TempDir) {
    let env = vec![
        (
            "MUTEKI_ADAPTER_ENDPOINT".into(),
            std::env::var("MUTEKI_LIVE_KIMI_ENDPOINT").expect("real Kimi endpoint"),
        ),
        (
            "KIMI_CODE_HOME".into(),
            std::env::var("MUTEKI_LIVE_KIMI_HOME").expect("isolated Kimi profile"),
        ),
    ];
    let cwd = tempfile::tempdir().unwrap();
    let handle = start_kimi_local_server(
        Path::new("kimi"),
        cwd.path(),
        None,
        &AccessPlan {
            mode: access.into(),
            selections: Vec::new(),
            auto_allow: Default::default(),
        },
        &env,
        None,
        &[],
        None,
    )
    .await
    .unwrap();
    (handle, cwd)
}

#[tokio::test]
#[ignore = "requires real Kimi Local Server configured only with source Ollama/Ark"]
async fn live_kimi_interrupt_steer_compaction() {
    let (handle, _cwd) = kimi(access::FULL_ACCESS).await;
    controls(&handle, true).await;
    handle
        .commands
        .send(SessionCommand::Shutdown)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires real Kimi Local Server configured only with source Ollama/Ark"]
async fn live_kimi_approval_and_question() {
    let (handle, _cwd) = kimi(access::SUPERVISED).await;
    send(&handle,1,"Use your Shell tool to execute `printf MUTEKI_PERMISSION_OK > runtime-permission-check.txt`. Then use AskUserQuestion to ask me to choose between Alpha and Beta. After I answer, reply with my choice and MUTEKI_INTERACTION_OK. These two native tools must both actually be used.").await;
    let mut approval = false;
    let mut question = false;
    let mut reply = String::new();
    let mut expected_questions = Vec::new();
    let mut resolved = false;
    loop {
        match next(&handle).await {
            AgentEvent::ApprovalRequested(request) => {
                approval = true;
                handle
                    .commands
                    .send(SessionCommand::RespondApproval {
                        request_id: request.id,
                        decision: ApprovalDecision::Option("approved".into()),
                    })
                    .await
                    .unwrap();
            }
            AgentEvent::UserInputRequested {
                request_id,
                questions,
                ..
            } => {
                question = true;
                expected_questions = questions.iter().map(|q| q.id.clone()).collect();
                let answers = questions
                    .into_iter()
                    .map(|q| {
                        (
                            q.id,
                            json!(
                                q.options
                                    .first()
                                    .map(|o| o.label.as_str())
                                    .unwrap_or("Alpha")
                            ),
                        )
                    })
                    .collect();
                handle
                    .commands
                    .send(SessionCommand::RespondUserInput {
                        request_id,
                        answers,
                    })
                    .await
                    .unwrap();
            }
            AgentEvent::UserInputResolved { answers, .. } => {
                resolved = expected_questions.iter().all(|id| answers.contains_key(id));
            }
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::AssistantMessage { text },
                ..
            }) => reply.push_str(&text),
            AgentEvent::TurnCompleted { status, .. } => {
                assert_eq!(status, TurnStatus::Completed);
                break;
            }
            AgentEvent::TurnFailed { code, message } => panic!("{code}: {message}"),
            AgentEvent::Error { message, .. } => panic!("interaction failed: {message}"),
            _ => {}
        }
    }
    handle
        .commands
        .send(SessionCommand::Shutdown)
        .await
        .unwrap();
    assert!(
        approval && question && resolved && reply.contains("MUTEKI_INTERACTION_OK"),
        "approval={approval} question={question} resolved={resolved} reply={reply}"
    );
}

#[tokio::test]
#[ignore = "requires real OMP CLI configured only with source Ollama/Ark"]
async fn live_omp_interrupt_steer_compaction() {
    let binary = PathBuf::from(std::env::var("MUTEKI_LIVE_OMP_BIN").expect("OMP binary"));
    let cwd = tempfile::tempdir().unwrap();
    let profile = cwd.path().join("profile");
    std::fs::create_dir_all(&profile).unwrap();
    let source = PathBuf::from(std::env::var("MUTEKI_LIVE_OMP_PROFILE").expect("source profile"));
    std::fs::copy(source.join("models.yml"), profile.join("models.yml")).unwrap();
    std::fs::write(
        profile.join("config.yml"),
        "compaction:\n  keepRecentTokens: 128\n  methodOrder: [soft]\n",
    )
    .unwrap();
    let model = std::env::var("MUTEKI_LIVE_OMP_MODEL").expect("source model");
    let env = vec![
        ("MUTEKI_OMP_PROVIDER".into(), "muteki".into()),
        ("MUTEKI_OMP_MODEL".into(), model.clone()),
        (
            "PI_CODING_AGENT_DIR".into(),
            profile.to_string_lossy().into_owned(),
        ),
    ];
    let handle = start_omp_rpc_v2(
        &binary,
        cwd.path(),
        Some(model),
        &env,
        &[],
        None,
        None,
        Vec::new(),
    )
    .await
    .unwrap();
    handle.commands.send(SessionCommand::Compact).await.unwrap();
    let mut error = false;
    loop {
        match next(&handle).await {
            AgentEvent::Error { message, .. } => {
                assert!(message.contains("Nothing to compact"));
                error = true;
            }
            AgentEvent::TurnCompleted { status, .. } => {
                assert!(error);
                assert_eq!(status, TurnStatus::Failed);
                break;
            }
            _ => {}
        }
    }
    eprintln!("empty native compaction rejected with a failed operation terminal");
    controls(&handle, false).await;
    handle
        .commands
        .send(SessionCommand::Shutdown)
        .await
        .unwrap();
}
