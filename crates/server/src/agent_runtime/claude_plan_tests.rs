use agent::{AgentEvent, LaunchEnv, SessionCommand, SessionOptions, TurnStatus};
use serde_json::{Value, json};

#[cfg(unix)]
#[test]
fn plan_mode_waits_for_cli_control_response_and_recovers_after_rejection() {
    if std::env::var_os("TCODE_CLAUDE_PLAN_FIXTURE_CHILD").is_some() {
        use std::io::{BufRead as _, Write as _};
        let log_path = std::env::var("TCODE_CLAUDE_PLAN_FIXTURE_LOG").unwrap();
        let mut log = std::fs::OpenOptions::new()
            .append(true)
            .open(log_path)
            .unwrap();
        let mut stdout = std::io::stdout().lock();
        let mut mode = "default".to_owned();
        let mut reject_once = true;
        for line in std::io::stdin().lock().lines() {
            let message: Value = serde_json::from_str(&line.unwrap()).unwrap();
            if message["type"] == "control_request"
                && message["request"]["subtype"] == "set_permission_mode"
            {
                let wanted = message["request"]["mode"].as_str().unwrap();
                writeln!(log, "control:{wanted}").unwrap();
                let response = if wanted == "plan" && reject_once {
                    reject_once = false;
                    json!({"type":"control_response","response":{"subtype":"error",
                        "request_id":message["request_id"],"error":"synthetic rejection"}})
                } else {
                    mode = wanted.to_owned();
                    if wanted == "acceptEdits" {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    json!({"type":"control_response","response":{"subtype":"success",
                        "request_id":message["request_id"],"response":{"permissionMode":mode}}})
                };
                writeln!(stdout, "{response}").unwrap();
                stdout.flush().unwrap();
            } else if message["type"] == "user" {
                writeln!(log, "prompt:{mode}").unwrap();
                writeln!(
                    stdout,
                    "{}",
                    json!({"type":"system","subtype":"init",
                    "session_id":"synthetic-claude","permissionMode":mode})
                )
                .unwrap();
                writeln!(
                    stdout,
                    "{}",
                    json!({"type":"result","subtype":"success",
                    "is_error":false,"result":"synthetic reply"})
                )
                .unwrap();
                stdout.flush().unwrap();
            }
        }
        return;
    }

    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!("tcode-claude-plan-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let log = root.join("wire.log");
    std::fs::write(&log, "").unwrap();
    let wrapper = root.join("claude-fixture");
    let executable = std::env::current_exe().unwrap();
    std::fs::write(&wrapper, format!(
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 2.1.213; exit 0; fi\nexec '{}' --exact agent_runtime::claude_plan_tests::plan_mode_waits_for_cli_control_response_and_recovers_after_rejection --nocapture\n",
        executable.display(),
    )).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    smol::block_on(async {
        let handle = agent::claude::start(SessionOptions {
            cwd: root.clone(),
            model: None,
            resume: None,
            fork: false,
            binary_path: Some(wrapper),
            option_selections: Vec::new(),
            mcp_servers: Vec::new(),
            launch_env: LaunchEnv {
                env: vec![
                    ("TCODE_CLAUDE_PLAN_FIXTURE_CHILD".into(), "1".into()),
                    (
                        "TCODE_CLAUDE_PLAN_FIXTURE_LOG".into(),
                        log.to_string_lossy().into_owned(),
                    ),
                ],
                home: None,
            },
            extra_args: Vec::new(),
            acp: None,
            instructions: None,
        })
        .await
        .unwrap();
        for (delivery_id, interaction, should_fail) in [
            (1, "plan", true),
            (2, "plan", false),
            (3, "default", false),
            (4, "default", false),
        ] {
            if delivery_id == 4 {
                handle
                    .commands
                    .send(SessionCommand::SetOption {
                        id: "permissionMode".into(),
                        value: json!("acceptEdits"),
                    })
                    .await
                    .unwrap();
            }
            handle
                .commands
                .send(SessionCommand::SendTurn {
                    delivery_id,
                    text: format!("synthetic turn {delivery_id}"),
                    attachments: Vec::new(),
                    options: Some(agent::TurnOptions {
                        interaction_mode: Some(interaction.into()),
                        ..Default::default()
                    }),
                })
                .await
                .unwrap();
            let mut events = Vec::new();
            loop {
                let event =
                    smol::future::or(async { Some(handle.events.recv().await.unwrap()) }, async {
                        smol::Timer::after(std::time::Duration::from_secs(8)).await;
                        None
                    })
                    .await
                    .expect("synthetic Claude control reply timed out");
                let mut event = event;
                while let AgentEvent::Origin { event: inner, .. } = event { event = *inner; }
                let done = matches!(event, AgentEvent::TurnCompleted { .. });
                events.push(event);
                if done {
                    break;
                }
            }
            assert_eq!(
                events
                    .iter()
                    .any(|event| matches!(event, AgentEvent::TurnAccepted { .. })),
                !should_fail
            );
            assert!(
                matches!(
                    events.last(),
                    Some(AgentEvent::TurnCompleted {
                        status: TurnStatus::Failed,
                        ..
                    })
                ) == should_fail
            );
            if should_fail {
                assert!(events.iter().any(
                    |event| matches!(event, AgentEvent::TurnFailed { code, message }
                    if code == "permission_mode_switch" && message.contains("synthetic rejection"))
                ));
            }
        }
        handle
            .commands
            .send(SessionCommand::Shutdown)
            .await
            .unwrap();
    });
    assert_eq!(
        std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        [
            "control:plan",
            "control:plan",
            "prompt:plan",
            "control:default",
            "prompt:default",
            "control:acceptEdits",
            "control:default",
            "prompt:default",
        ]
    );
    std::fs::remove_dir_all(root).unwrap();
}
