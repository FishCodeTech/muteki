use super::*;
use crate::app::test_support::*;
use serde_json::{Value, json};
use std::sync::Mutex;
use tcode_core::pull_request::{PullRequestReviewDraftEdit, PullRequestStack};
use tcode_protocol::Command;
use tcode_services::{
    github::{Credentials, GitHub, GitHubApi},
    settings::SettingsStore,
};

use crate::app::test_support::github_fixture as fixture;

/// Sends one command and runs the host until it answers it.
fn acked(
    state: &TestEntity,
    cx: &mut TestAppContext,
    id: u64,
    command: Command,
) -> Result<CommandResponse, tcode_protocol::ProtocolError> {
    state.dispatch_command(cx, id, command);
    cx.run_until_sent(|message| match message {
        tcode_protocol::HostMessage::Ack { id: acked, result } if *acked == id => {
            Some(result.clone())
        }
        _ => None,
    })
    .0
}

struct HostReply {
    state: &'static str,
    stack: bool,
    requests: Vec<Value>,
}
fn pr(number: u64, state: &str, stack: bool) -> Value {
    let terminal = chrono::Utc::now().to_rfc3339();
    json!({
        "number": number,
        "url": format!("https://github.com/sample/project/pull/{number}"),
        "title": format!("Change {number}"),
        "state": state,
        "isDraft": false,
        "headRefName": format!("layer-{number}"),
        "baseRefName": "main",
        "updatedAt": "2026-10-08T00:00:00Z",
        "mergedAt": (state == "MERGED").then_some(&terminal),
        "closedAt": (state != "OPEN").then_some(&terminal),
        "additions": 3,
        "deletions": 1,
        "changedFiles": 1,
        "reviewDecision": "APPROVED",
        "mergeable": "MERGEABLE",
        "stack": if stack { json!({"number": 7}) } else { Value::Null },
    })
}
fn client(store: &SessionStore, fixture: &fixture::Fixture) -> Arc<dyn Forge> {
    client_as(store, fixture, "fixture")
}
/// The host reading GitHub with `token`, which names the account.
fn client_as(store: &SessionStore, fixture: &fixture::Fixture, token: &str) -> Arc<dyn Forge> {
    GitHub::new(GitHubApi::new(
        Credentials::new(
            SettingsStore::new(store.root().to_path_buf()),
            [("GH_TOKEN".into(), token.into())],
        ),
        fixture.builder(),
    ))
}
fn linked_meta(id: &str, settled: bool) -> SessionMeta {
    let mut meta = SessionMeta::new(
        ProviderKind::Codex,
        PathBuf::from("/tmp/synthetic-checkout"),
        None,
    );
    meta.id = id.into();
    meta.project_id = Some("sample".into());
    meta.settled_at = settled.then_some(1);
    pull_request::link_pull_request(
        &mut meta.pull_requests,
        PullRequestKey::new("github.com", "sample/project", 1),
        "https://github.com/sample/project/pull/1".into(),
        PullRequestSource::Manual,
        1,
        true,
    );
    meta
}
fn sweep(state: &TestEntity, cx: &mut TestAppContext) {
    // A sweep that finds a sync still running is skipped; let that one finish.
    cx.run_until(|state| !state.pull_requests.syncing);
    state.update(cx, |state, cx| {
        state.sweep_pull_requests(false, cx).detach()
    });
    cx.run_until(|state| !state.pull_requests.syncing);
}

fn linked(id: &str, number: u64, settled: bool) -> SessionMeta {
    let mut meta = linked_meta(id, settled);
    meta.pull_requests[0].key.number = number;
    meta.pull_requests[0].url = format!("https://github.com/sample/project/pull/{number}");
    meta
}
fn command(command: &str) -> AgentEvent {
    AgentEvent::ItemCompleted(ThreadItem {
        id: "command".into(),
        parent_item_id: None,
        content: ItemContent::CommandExecution {
            command: command.into(),
            output: String::new(),
            exit_code: Some(0),
            status: ItemStatus::Completed,
        },
    })
}
fn turn_completed() -> AgentEvent {
    AgentEvent::TurnCompleted {
        turn_id: "turn".into(),
        status: TurnStatus::Completed,
        usage: None,
    }
}

#[test]
fn shared_sync_changes_only_observations_and_honors_terminal_cadence() {
    let store = TestStore::new("tcode-pr-sync");
    let fixture = fixture::Fixture::new();
    let api = client(&store, &fixture);
    let response = Arc::new(Mutex::new(HostReply {
        state: "OPEN",
        stack: false,
        requests: Vec::new(),
    }));
    let responding = response.clone();
    let _server = fixture.serve(move |exchange| {
        let sent: Value = serde_json::from_slice(&exchange.body).unwrap();
        let mut model = responding.lock().unwrap();
        model.requests.push(sent.clone());
        let mut data = serde_json::Map::new();
        for (name, number) in sent["variables"].as_object().unwrap() {
            if let Some(alias) = name.strip_suffix("_number") {
                data.insert(
                    alias.into(),
                    json!({"pullRequest":pr(number.as_u64().unwrap(),model.state,model.stack)}),
                );
            }
        }
        exchange.reply(200, "", &serde_json::to_vec(&json!({"data":data})).unwrap());
    });
    let read_numbers = || -> Vec<u64> {
        response
            .lock()
            .unwrap()
            .requests
            .iter()
            .flat_map(|sent| {
                sent["variables"]
                    .as_object()
                    .unwrap()
                    .iter()
                    .filter(|(name, _)| name.ends_with("_number"))
                    .map(|(_, number)| number.as_u64().unwrap())
                    .collect::<Vec<_>>()
            })
            .collect()
    };
    let mut cx = TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(&mut cx, |state, _| {
        state.pull_requests = PullRequestRuntime::new(api);
        for meta in [
            linked("active", 1, false),
            linked("settled", 1, true),
            linked("settled-alone", 2, true),
        ] {
            store.upsert_meta(&meta).unwrap();
            state.sessions.push(meta);
        }
    });
    acked(
        &state,
        &mut cx,
        1,
        Command::SetAutoSettle {
            session_id: "active".into(),
            enabled: false,
        },
    )
    .unwrap();
    sweep(&state, &mut cx);
    let mut first_reads = read_numbers();
    first_reads.sort_unstable();
    assert_eq!(
        first_reads,
        vec![1, 2],
        "a PR shared by two threads needs one read, and a settled thread's unsynced link is read"
    );
    let first = store.load_index().unwrap();
    for id in ["active", "settled"] {
        assert!(first.iter().any(|meta| {
            meta.id == id
                && meta.pull_requests[0]
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| s.state == PullRequestState::Open)
        }));
    }
    sweep(&state, &mut cx);
    assert_eq!(
        store.load_index().unwrap(),
        first,
        "unchanged state must preserve activity, unread state and synced_at"
    );
    response.lock().unwrap().state = "CLOSED";
    state.update(&mut cx, |state, cx| {
        state.on_event("active", command("cd project&&gh pr close 1"), cx);
        state.on_event("active", turn_completed(), cx);
    });
    cx.run_until(|state| {
        !state.pull_requests.syncing
            && state.find_meta("active").unwrap().pull_requests[0]
                .snapshot
                .as_ref()
                .is_some_and(|s| s.state == PullRequestState::Closed)
    });
    let closed_reads = read_numbers().len();
    sweep(&state, &mut cx);
    assert_eq!(read_numbers().len(), closed_reads);
    let fifteen_minutes_pass = |state: &TestEntity, cx: &mut TestAppContext| {
        state.update(cx, |state, _| {
            state.pull_requests.last_synced.insert(
                PullRequestKey::new("github.com", "sample/project", 1),
                now_secs() - 900,
            );
        })
    };
    fifteen_minutes_pass(&state, &mut cx);
    response.lock().unwrap().state = "MERGED";
    sweep(&state, &mut cx);
    assert_eq!(read_numbers().len(), closed_reads + 1);
    assert!(state.read(|state| {
        state.find_meta("active").unwrap().pull_requests[0]
            .snapshot
            .as_ref()
            .is_some_and(|s| s.state == PullRequestState::Merged)
    }));
    fifteen_minutes_pass(&state, &mut cx);
    sweep(&state, &mut cx);
    assert_eq!(read_numbers().len(), closed_reads + 1, "merged stops reads");
    assert_eq!(
        read_numbers().iter().filter(|number| **number == 2).count(),
        1,
        "a settled thread's open link nobody shares is not independently due"
    );
    assert!(state.read(|state| {
        state.find_meta("settled-alone").unwrap().pull_requests[0]
            .snapshot
            .as_ref()
            .is_some_and(|s| s.state == PullRequestState::Open)
    }));
}

#[test]
fn native_stack_sync_preserves_dismissals_and_explicit_restore() {
    let store = TestStore::new("tcode-pr-stack");
    let fixture = fixture::Fixture::new();
    let api = client(&store, &fixture);
    let _server = fixture.serve(|exchange| {
        if exchange.request.starts_with("GET ") {
            exchange.reply(
                200,
                "",
                br#"[{"id":"stack-7","number":7,"url":"https://github.com/sample/project/stack/7","base":{"ref":"main"},"pull_requests":[{"number":1,"head":{"ref":"layer-1"},"state":"merged"},{"number":2,"head":{"ref":"layer-2"},"state":"open"}]}]"#,
            );
        } else {
            let sent: Value = serde_json::from_slice(&exchange.body).unwrap();
            let mut data = serde_json::Map::new();
            for (name, number) in sent["variables"].as_object().unwrap() {
                if let Some(alias) = name.strip_suffix("_number") {
                    let state = if number == 1 { "MERGED" } else { "OPEN" };
                    data.insert(
                        alias.into(),
                        json!({"pullRequest": pr(number.as_u64().unwrap(), state, true)}),
                    );
                }
            }
            exchange.reply(200, "", &serde_json::to_vec(&json!({"data":data})).unwrap());
        }
    });
    let mut cx = TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(&mut cx, |state, _| {
        state.pull_requests = PullRequestRuntime::new(api);
        state
            .sessions
            .extend([linked_meta("active", false), linked("settled", 2, true)]);
    });
    let settle_requests = |cx: &mut TestAppContext| {
        cx.run_until(|state| {
            !state.pull_requests.syncing
                && !state.pull_requests.sync_scheduled
                && state.pull_requests.requested.is_empty()
        })
    };
    sweep(&state, &mut cx);
    settle_requests(&mut cx);
    assert_eq!(
        state.read(|state| state.find_meta("active").unwrap().pull_requests.len()),
        2
    );
    let settled = state.read(|state| state.find_meta("settled").unwrap().pull_requests);
    assert!(
        settled.len() == 1 && settled[0].snapshot.is_some(),
        "settled threads receive the snapshot without installing siblings"
    );
    state.update(&mut cx, |state, cx| {
        let mut meta = state.find_meta("settled").unwrap();
        meta.settled_at = None;
        state.save_pull_request_meta(meta, cx);
    });
    sweep(&state, &mut cx);
    settle_requests(&mut cx);
    assert_eq!(
        state.read(|state| state.find_meta("settled").unwrap().pull_requests.len()),
        2,
        "known topology expands on the next read once the thread is unsettled"
    );
    let key = PullRequestKey::new("github.com", "sample/project", 2);
    let layer = |state: &TestEntity| {
        state.read(|state| {
            state
                .find_meta("active")
                .unwrap()
                .pull_requests
                .into_iter()
                .find(|link| link.key == key)
                .unwrap()
        })
    };
    state.update(&mut cx, |state, cx| {
        state.unlink_pull_request("active", &key, cx);
    });
    sweep(&state, &mut cx);
    settle_requests(&mut cx);
    assert_eq!(
        serde_json::to_value(layer(&state)).unwrap(),
        json!({"key":{"host":"github.com","repository":"sample/project","number":2},"source":"dismissed"})
    );
    state.update(&mut cx, |state, cx| {
        state
            .apply_pull_request_link(
                "active",
                key.clone(),
                "https://github.com/sample/project/pull/2".into(),
                PullRequestSource::Manual,
                true,
                cx,
            )
            .unwrap()
    });
    settle_requests(&mut cx);
    let restored = layer(&state);
    assert!(restored.visible() && restored.snapshot.is_some());
    assert_eq!(restored.source, PullRequestSource::Manual);
    assert!(matches!(
        restored.stack,
        PullRequestStackState::Native(PullRequestStack { number: 7, .. })
    ));
}

#[test]
fn a_synced_terminal_pull_request_settles_only_its_thread_once_its_stack_allows() {
    let store = TestStore::new("tcode-pr-settlement");
    let fixture = fixture::Fixture::new();
    let api = client(&store, &fixture);
    let _server = fixture.serve(|exchange| {
        if exchange.request.starts_with("GET ") {
            exchange.reply(
                200,
                "",
                br#"[{"id":"stack-7","number":7,"url":"https://github.com/sample/project/stack/7","base":{"ref":"main"},"pull_requests":[{"number":3,"head":{"ref":"layer-3"},"state":"merged"},{"number":4,"head":{"ref":"layer-4"},"state":"open"}]}]"#,
            );
        } else {
            let sent: Value = serde_json::from_slice(&exchange.body).unwrap();
            let mut data = serde_json::Map::new();
            for (name, number) in sent["variables"].as_object().unwrap() {
                if let Some(alias) = name.strip_suffix("_number") {
                    let number = number.as_u64().unwrap();
                    let state = if number == 4 { "OPEN" } else { "MERGED" };
                    data.insert(
                        alias.into(),
                        json!({"pullRequest": pr(number, state, number >= 3)}),
                    );
                }
            }
            exchange.reply(200, "", &serde_json::to_vec(&json!({"data":data})).unwrap());
        }
    });
    let worked_at = now_millis() - 3_600_000;
    let activity = tcode_core::settlement::ThreadActivity {
        last_message_at: Some(worked_at - 60_000),
        last_human_message_at: Some(worked_at - 60_000),
        last_run_started_at: Some(worked_at - 59_000),
        last_run_completed_at: Some(worked_at),
        failed: false,
        interrupted: false,
    };
    let mut cx = TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    acked(
        &state,
        &mut cx,
        1,
        Command::PatchSettings {
            patch: tcode_protocol::SettingsPatch::AutoSettleOnMerge(true),
        },
    )
    .unwrap();
    state.update(&mut cx, |state, _| {
        state.pull_requests = PullRequestRuntime::new(api);
        for meta in [linked("merged", 1, false), linked("stacked", 3, false)] {
            store.upsert_meta(&meta).unwrap();
            state
                .thread_activity
                .insert(meta.id.clone(), activity.clone());
            state.sessions.push(meta);
        }
    });
    sweep(&state, &mut cx);
    cx.run_until(|state| {
        !state.pull_requests.syncing
            && !state.pull_requests.sync_scheduled
            && state.pull_requests.requested.is_empty()
    });
    let merged = state.read(|state| state.find_meta("merged").unwrap());
    assert!(merged.is_settled());
    assert_eq!(merged.settled_at, Some(worked_at / 1000));
    let stacked = state.read(|state| state.find_meta("stacked").unwrap());
    assert_eq!(stacked.pull_requests.len(), 2);
    assert!(
        !stacked.is_settled(),
        "the merged layer's open sibling is linked before it is evaluated"
    );
}

#[test]
fn rate_limit_keeps_requests_due_until_host_pause_expires() {
    let store = TestStore::new("tcode-pr-paused");
    let fixture = fixture::Fixture::new();
    let api = client(&store, &fixture);
    let unpaused_api = client(&store, &fixture);
    let calls = Arc::new(Mutex::new(0));
    let serving = calls.clone();
    let _server = fixture.serve(move |exchange| {
        let mut calls = serving.lock().unwrap();
        *calls += 1;
        if *calls == 1 {
            exchange.reply(429, "retry-after: 3600\r\n", b"{}");
        } else {
            exchange.reply(
                200,
                "",
                &serde_json::to_vec(&json!({"data":{"s0":{"pullRequest":pr(1,"OPEN",false)}}}))
                    .unwrap(),
            );
        }
    });
    let mut cx = TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let key = PullRequestKey::new("github.com", "sample/project", 1);
    state.update(&mut cx, |state, cx| {
        state.pull_requests = PullRequestRuntime::new(api);
        let mut meta = linked_meta("active", false);
        meta.pull_requests.clear();
        state.sessions.push(meta);
        state
            .apply_pull_request_link(
                "active",
                key.clone(),
                "https://github.com/sample/project/pull/1".into(),
                PullRequestSource::Manual,
                true,
                cx,
            )
            .unwrap();
    });
    cx.run_until(|state| !state.pull_requests.syncing && !state.pull_requests.paused.is_empty());
    assert!(state.read(|state| matches!(
        state.find_meta("active").unwrap().pull_requests[0].sync_error,
        Some(PullRequestSyncError::RateLimited { retry_at }) if retry_at >= now_secs() + 3_500
    )));
    sweep(&state, &mut cx);
    assert_eq!(*calls.lock().unwrap(), 1, "a paused group makes no request");
    assert!(state.read(|state| {
        state.find_meta("active").unwrap().pull_requests[0]
            .snapshot
            .is_none()
            && state.pull_requests.requested.contains_key(&key)
    }));
    // The hour passes: both the runtime's group pause and the transport's host pause
    // (private to a client, so a client whose pause has elapsed) are behind the clock.
    state.update(&mut cx, |state, _| {
        for until in state.pull_requests.paused.values_mut() {
            *until = SystemTime::now() - Duration::from_secs(1);
        }
        state.pull_requests.forge = unpaused_api;
    });
    sweep(&state, &mut cx);
    assert_eq!(*calls.lock().unwrap(), 2);
    assert!(state.read(|state| {
        let link = &state.find_meta("active").unwrap().pull_requests[0];
        link.snapshot.is_some()
            && link.sync_error.is_none()
            && state.pull_requests.requested.is_empty()
    }));
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = tcode_services::process::command("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

#[test]
fn mcp_child_linking_is_bound_to_its_token_and_rejects_a_thread_override() {
    let store = TestStore::new("tcode-pr-mcp");
    let cwd = store.root().join("checkout");
    std::fs::create_dir(&cwd).unwrap();
    git(&cwd, &["init", "-b", "main"]);
    git(
        &cwd,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/sample/project.git",
        ],
    );
    let settings_store = SettingsStore::new(store.root().clone());
    let mut settings = settings_store.load();
    settings.source_control.hosts.insert(
        "github.com".into(),
        tcode_core::settings::HostSettings {
            enabled: false,
            ..tcode_core::settings::HostSettings::new(pull_request::HostKind::Github)
        },
    );
    settings_store.save(&settings).unwrap();
    let mut cx = TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let mut host = mcp_host::Host::bind().unwrap();
    let server = pull_request_mcp::start(&mut host);
    let registration = state.update(&mut cx, |state, cx| {
        state.pump_pull_request_requests(Some(server), cx);
        let mut parent = linked_meta("parent", false);
        parent.cwd = cwd;
        parent.pull_requests.clear();
        let mut child = parent.clone();
        child.id = "child".into();
        child.parent_session_id = Some(parent.id.clone());
        state.sessions.extend([parent, child.clone()]);
        state.pull_request_registration_for(&child).unwrap()
    });
    host.start().unwrap();
    let result = Arc::new(Mutex::new(None));
    let completed = result.clone();
    let job = std::thread::spawn(move || {
        let (init, session) = rpc(
            &registration.url,
            &registration.bearer_token,
            None,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"contract-fixture","version":"1"}}}),
        );
        assert!(
            init["result"]["capabilities"]["tools"].is_object(),
            "initialize response: {init}"
        );
        rpc(
            &registration.url,
            &registration.bearer_token,
            session.as_deref(),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        );
        let call = |id, name, args| {
            rpc(
                &registration.url,
                &registration.bearer_token,
                session.as_deref(),
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "tools/call",
                    "params": {"name": name, "arguments": args},
                }),
            )
            .0
        };
        let linked = call(
            2,
            "link_pull_request",
            json!({"url":"https://github.com/sample/project/pull/1?view=1#issuecomment-7"}),
        );
        let forbidden = call(
            3,
            "link_pull_request",
            json!({"url":"https://github.com/sample/project/pull/2","threadId":"parent"}),
        );
        let listed = call(4, "list_thread_pull_requests", json!({}));
        let unlinked = call(
            5,
            "unlink_pull_request",
            json!({"repository":"sample/project","number":1}),
        );
        *completed.lock().unwrap() = Some((linked, forbidden, listed, unlinked));
    });
    cx.run_until(|_| result.lock().unwrap().is_some());
    job.join().unwrap();
    let (linked, forbidden, listed, unlinked) = result.lock().unwrap().take().unwrap();
    assert_eq!(
        linked["result"]["isError"], false,
        "link response: {linked}"
    );
    assert!(forbidden["error"].is_object() || forbidden["result"]["isError"] == true);
    let listed: Value =
        serde_json::from_str(listed["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(listed["pullRequests"][0]["number"], 1);
    assert_eq!(listed["pullRequests"][0]["source"], "agent");
    assert_eq!(
        unlinked["result"]["isError"], false,
        "unlink response: {unlinked}"
    );
    assert!(state.read(|state| {
        state.find_meta("parent").unwrap().pull_requests.is_empty()
            && state
                .find_meta("child")
                .unwrap()
                .pull_requests
                .iter()
                .all(|link| !link.visible())
    }));
}

/// The linking instructions reach the provider at launch, for its own instruction channel; no
/// turn carries them, so the stored user turn is the user's text alone, and what the agent sees
/// holds the delivered text verbatim under that channel. A provider that took none of Tcode's MCP
/// servers is shown the tools as offered, with no instructions.
#[test]
fn instructions_reach_the_provider_at_launch_and_no_turn_carries_them() {
    use tcode_protocol::{InjectedChannel, InjectedInstructions};
    let store = TestStore::new("tcode-pr-instructions");
    let mut host = mcp_host::Host::bind().unwrap();
    let server = pull_request_mcp::start(&mut host);
    let scripted = scripted_provider(ProviderKind::ClaudeCode);
    let (launched_tx, launched) = smol::channel::unbounded();
    let scripted_launcher = scripted.launcher.clone();
    let launcher = ProviderLauncher(Arc::new(move |kind, options: agent::SessionOptions| {
        launched_tx.try_send(options.clone()).unwrap();
        scripted_launcher.launch(kind, options)
    }));
    let mut cx = TestAppContext::default();
    let state = cx.new_entity({
        let mut state = TestClientState::new((*store).clone());
        state.set_provider_launcher_for_test(launcher);
        state
    });
    state.update(&mut cx, |state, cx| {
        state.pump_pull_request_requests(Some(server), cx);
        let mut meta = SessionMeta::new(
            ProviderKind::ClaudeCode,
            PathBuf::from("/tmp/synthetic-checkout"),
            None,
        );
        meta.id = "thread".into();
        store.upsert_meta(&meta).unwrap();
        state.sessions.push(meta);
        state.select_session("thread", cx);
    });
    state.dispatch_command(
        &mut cx,
        1,
        Command::SendTurn {
            session_id: "thread".into(),
            text: "fix the build".into(),
            attachment_paths: Vec::new(),
        },
    );
    let commands = scripted.commands.clone();
    cx.run_until(|_| !commands.is_empty());
    let Ok(SessionCommand::SendTurn {
        text, delivery_id, ..
    }) = commands.try_recv()
    else {
        panic!("expected a provider delivery")
    };
    assert_eq!(text, "fix the build");
    let options = launched.try_recv().unwrap();
    let linking = pull_request::linking_instructions(&pull_request::hosts_in([]));
    assert_eq!(options.instructions.as_deref(), Some(linking.as_str()));
    assert!(
        options
            .mcp_servers
            .iter()
            .any(|server| server.name == "tcode_pull_requests")
    );

    scripted
        .events
        .try_send(AgentEvent::TurnAccepted { delivery_id })
        .unwrap();
    cx.run_until(|state| {
        state
            .resident("thread")
            .is_some_and(|session| session.delivery_in_flight.is_none())
    });
    let stored = store
        .read_events("thread")
        .unwrap()
        .into_iter()
        .find_map(|event| match event.event {
            AgentEvent::ItemCompleted(ThreadItem {
                content:
                    ItemContent::UserMessage {
                        text, context_len, ..
                    },
                ..
            }) => Some((text, context_len)),
            _ => None,
        });
    assert_eq!(stored, Some(("fix the build".to_owned(), None)));

    let disclosed = |state: &TestEntity| {
        state.read(|state| {
            state
                .session_status_snapshot("thread")
                .unwrap()
                .injected_tools
                .unwrap()
        })
    };
    let tools = disclosed(&state);
    assert!(tools.attached);
    assert_eq!(
        tools.instructions,
        [
            InjectedInstructions {
                channel: InjectedChannel::Linking(agent::InstructionChannel::SessionAppend),
                text: linking,
            },
            InjectedInstructions {
                channel: InjectedChannel::McpServer("tcode_pull_requests".into()),
                text: pull_request_mcp::INSTRUCTIONS.into(),
            },
        ]
    );

    scripted
        .events
        .try_send(AgentEvent::McpServersNotAttached)
        .unwrap();
    cx.run_until(|state| {
        let tools = state.session_status_snapshot("thread").unwrap();
        !tools.injected_tools.unwrap().attached
    });
    let tools = disclosed(&state);
    assert!(tools.instructions.is_empty());
    assert!(!tools.tools.is_empty());
}

/// A host started the way every composition root starts one discloses the tools to each client,
/// for the providers that receive them.
#[test]
fn the_agent_tools_are_disclosed_before_the_first_turn() {
    let store = TestStore::new("tcode-pr-tools-disclosed");
    let mut services = crate::pipe::HostServices::default();
    services.start_mcp_servers(false).unwrap();
    let host = crate::pipe::spawn_host((*store).clone(), services).unwrap();
    for (provider, disclosed) in [(ProviderKind::ClaudeCode, true), (ProviderKind::Pi, false)] {
        let tools = smol::block_on(host.update_state_for_test(move |state, _| {
            let mut meta =
                SessionMeta::new(provider, PathBuf::from("/tmp/synthetic-checkout"), None);
            meta.orchestrate_enabled = true;
            let id = meta.id.clone();
            state.sessions.push(meta.clone());
            state
                .residents
                .live
                .insert(id.clone(), ActiveSession::new(meta, false, Vec::new()));
            state.session_status_snapshot(&id).unwrap().injected_tools
        }))
        .unwrap();
        assert_eq!(
            tools.is_some(),
            disclosed,
            "{provider:?}: shown exactly when its provider would receive the tools"
        );
        if let Some(tools) = tools {
            let description = orchestrate_mcp::tool_descriptions()
                .into_iter()
                .find(|(name, _)| name == "settle_all")
                .unwrap();
            assert!(tools.tools.contains(&description));
        }
    }
    host.shutdown_blocking().unwrap();
}

/// The tools a thread was registered with keep the hosts they named, so what the agent sees
/// matches what it has when a host is added mid-thread; a later thread names the new host.
#[test]
fn a_threads_tools_keep_the_hosts_they_were_registered_with() {
    let store = TestStore::new("tcode-pr-tools-hosts");
    let mut cx = TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let mut host = mcp_host::Host::bind().unwrap();
    let server = pull_request_mcp::start(&mut host);
    state.update(&mut cx, |state, cx| {
        state.pump_pull_request_requests(Some(server), cx)
    });
    let listed = |state: &mut AppState, id: &str| {
        state
            .session_status_snapshot(id)
            .unwrap()
            .injected_tools
            .unwrap()
            .tools
            .into_iter()
            .find(|(name, _)| name == "list_thread_pull_requests")
            .unwrap()
            .1
    };
    state.update(&mut cx, |state, _| {
        let meta = SessionMeta::new(
            ProviderKind::ClaudeCode,
            PathBuf::from("/tmp/synthetic-checkout"),
            None,
        );
        let before = meta.id.clone();
        state.pull_request_registration_for(&meta).unwrap();
        state.sessions.push(meta.clone());
        state.install_selected(ActiveSession::new(meta, false, Vec::new()));
        state.settings.source_control.hosts.insert(
            "codeberg.org".into(),
            tcode_core::settings::HostSettings::new(pull_request::HostKind::Forgejo),
        );
        state.name_pull_request_hosts();
        assert!(listed(state, &before).contains("without asking GitHub."));
        let meta = SessionMeta::new(
            ProviderKind::ClaudeCode,
            PathBuf::from("/tmp/synthetic-checkout"),
            None,
        );
        let after = meta.id.clone();
        state.pull_request_registration_for(&meta).unwrap();
        state.sessions.push(meta.clone());
        state.install_selected(ActiveSession::new(meta, false, Vec::new()));
        let text = listed(state, &after);
        assert!(text.contains("without asking GitHub or Forgejo."), "{text}");
    });
}

#[test]
fn reads_answer_only_a_linked_pull_request_and_a_synced_change_reads_it_fresh() {
    let store = TestStore::new("tcode-pr-read");
    let fixture = fixture::Fixture::new();
    let api = client(&store, &fixture);
    // The pull request's last update, and every GraphQL operation the host sent.
    let model = Arc::new(Mutex::new(("2026-10-08T00:00:00Z", Vec::<String>::new())));
    let responding = model.clone();
    let _server = fixture.serve(move |exchange| {
        let sent: Value = serde_json::from_slice(&exchange.body).unwrap();
        let operation = sent["query"]
            .as_str()
            .unwrap()
            .split(['(', ' '])
            .nth(1)
            .unwrap();
        let mut model = responding.lock().unwrap();
        model.1.push(operation.to_owned());
        let reply = match operation {
            "PullRequestSummaries" => {
                let mut row = pr(1, "OPEN", false);
                row["updatedAt"] = json!(model.0);
                json!({"data": {"s0": {"pullRequest": row}}})
            }
            "PullRequestConversation" => json!({"data": {"repository": {"pullRequest": {
                "id": "PR_1", "body": "Description", "createdAt": "2026-10-01T00:00:00Z",
                "url": "https://github.com/sample/project/pull/1", "author": {"login": "octocat"},
                "comments": {"pageInfo": {"hasNextPage": false}, "nodes": []},
                "reviews": {"pageInfo": {"hasNextPage": false}, "nodes": []},
            }}}}),
            _ => json!({"data": {"repository": {"pullRequest": {"reviewThreads": {
                "pageInfo": {"hasNextPage": false}, "nodes": [],
            }}}}}),
        };
        exchange.reply(200, "", &serde_json::to_vec(&reply).unwrap());
    });
    let conversations = || {
        model
            .lock()
            .unwrap()
            .1
            .iter()
            .filter(|operation| *operation == "PullRequestConversation")
            .count()
    };
    let mut cx = TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(&mut cx, |state, _| {
        state.pull_requests = PullRequestRuntime::new(api);
        let meta = linked("active", 1, false);
        store.upsert_meta(&meta).unwrap();
        state.sessions.push(meta);
    });
    let mut next_id = 0;
    let mut read = |cx: &mut TestAppContext, number: u64| {
        next_id += 1;
        state.deliver(
            cx,
            next_id,
            tcode_protocol::ClientPayload::Query(tcode_protocol::Query::PullRequest {
                session_id: "active".into(),
                key: PullRequestKey::new("github.com", "sample/project", number),
                read: tcode_protocol::PullRequestRead::Conversation,
            }),
        );
        let read_id = next_id;
        cx.run_until_sent(|message| match message {
            tcode_protocol::HostMessage::QueryResult { id, result } if *id == read_id => {
                Some(result.clone())
            }
            _ => None,
        })
        .0
    };

    sweep(&state, &mut cx);
    assert!(read(&mut cx, 1).is_ok());
    assert!(read(&mut cx, 1).is_ok());
    assert_eq!(conversations(), 1, "a second read within the TTL is shared");
    assert_eq!(
        read(&mut cx, 2).unwrap_err().code,
        "pull_request_not_linked",
        "a pull request the thread does not link is not read through it"
    );
    assert_eq!(conversations(), 1);

    sweep(&state, &mut cx);
    read(&mut cx, 1).unwrap();
    assert_eq!(conversations(), 1, "an unchanged pull request stays shared");
    model.lock().unwrap().0 = "2026-10-09T00:00:00Z";
    state.update(&mut cx, |state, cx| {
        state.request_pull_request_sync(PullRequestKey::new("github.com", "sample/project", 1), cx)
    });
    cx.run_until(|state| {
        state.find_meta("active").unwrap().pull_requests[0]
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.updated_at == "2026-10-09T00:00:00Z")
    });
    read(&mut cx, 1).unwrap();
    assert_eq!(conversations(), 2, "a change the sync saw is read fresh");

    // The thread shows every layer of the native stack its link carries.
    state.update(&mut cx, |state, _| {
        let layer = |number: u64| tcode_core::pull_request::PullRequestStackLayer {
            url: format!("https://github.com/sample/project/pull/{number}"),
            number,
            head_branch: format!("layer-{number}"),
            state: PullRequestState::Open,
        };
        let meta = state
            .sessions
            .iter_mut()
            .find(|meta| meta.id == "active")
            .unwrap();
        meta.pull_requests[0].stack = PullRequestStackState::Native(PullRequestStack {
            id: "stack".into(),
            number: 7,
            url: "https://github.com/sample/project/stacks/7".into(),
            base: "main".into(),
            layers: vec![layer(1), layer(3)],
        });
    });
    assert!(
        read(&mut cx, 3).is_ok(),
        "a layer of the linked pull request's stack is read through the thread"
    );
    assert_eq!(
        read(&mut cx, 4).unwrap_err().code,
        "pull_request_not_linked",
        "a pull request in no stack the thread shows is still refused"
    );
    assert_eq!(conversations(), 3);
}

#[test]
fn a_review_draft_is_the_hosts_across_a_restart_and_a_moved_head_keeps_it_until_moved() {
    const BASE: &str = "3333333333333333333333333333333333333333";
    const OLD: &str = "1111111111111111111111111111111111111111";
    const NEW: &str = "2222222222222222222222222222222222222222";
    let dir = TestStore::new("tcode-pr-review");
    let fixture = fixture::Fixture::new();
    let api = client(&dir, &fixture);
    // The head GitHub reports, and every request line and body it saw.
    let model = Arc::new(Mutex::new((OLD, Vec::<(String, String)>::new())));
    let responding = model.clone();
    let _server = fixture.serve(move |exchange| {
        let line = exchange.request.lines().next().unwrap().to_owned();
        let diff = exchange.request.contains("application/vnd.github.diff");
        let body = String::from_utf8_lossy(&exchange.body).into_owned();
        let mut model = responding.lock().unwrap();
        model.1.push((line.clone(), body.clone()));
        let head = model.0;
        // Line 8 is rewritten at the new head; every other line reads the same.
        let text = (1..=12)
            .map(|line| match (line, head) {
                (8, NEW) => "changed".to_owned(),
                _ => format!("line {line}"),
            })
            .collect::<Vec<_>>()
            .join("\n");
        let reply = if diff {
            "diff --git a/src/lib.rs b/src/lib.rs\nindex 1111111..2222222 100644\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,10 +1,10 @@\n line 1\n".to_owned()
        } else if line.starts_with("GET /repos/sample/project/pulls/1 ") {
            json!({"base": {"sha": BASE}, "head": {"sha": head}, "changed_files": 1, "node_id": "PR_1"})
                .to_string()
        } else if line.starts_with("GET /repos/sample/project/contents/src/lib.rs?ref=") {
            if line.contains(NEW) {
                text
            } else {
                (1..=12)
                    .map(|line| format!("line {line}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        } else if body.contains("PullRequestSummaries") {
            json!({"data": {"s0": {"pullRequest": pr(1, "OPEN", false)}}}).to_string()
        } else {
            json!({"id": 1}).to_string()
        };
        exchange.reply(200, "", reply.as_bytes());
    });
    let key = PullRequestKey::new("github.com", "sample/project", 1);
    let mut cx = TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*dir).clone()));
    state.update(&mut cx, |state, _| {
        state.pull_requests = PullRequestRuntime::new(api.clone());
        let meta = linked("active", 1, false);
        dir.upsert_meta(&meta).unwrap();
        state.sessions.push(meta);
    });
    let mut next_id = 0;
    let mut command = |state: &TestEntity, cx: &mut TestAppContext, command: Command| {
        next_id += 1;
        acked(state, cx, next_id, command)
    };
    let edit = |edit| Command::EditPullRequestReviewDraft {
        session_id: "active".into(),
        key: key.clone(),
        edit,
    };
    let add = |line: u32| {
        edit(PullRequestReviewDraftEdit::AddComment {
            head: OLD.into(),
            revision: OLD.into(),
            path: "src/lib.rs".into(),
            side: tcode_core::session::ReviewSide::New,
            start_line: line,
            end_line: line,
            body: format!("Line {line}"),
        })
    };
    let drafts = |state: &TestEntity| {
        state.read(|state| state.find_meta("active").unwrap().pull_request_reviews)
    };
    command(&state, &mut cx, add(3)).unwrap();
    command(&state, &mut cx, add(8)).unwrap();
    assert_eq!(
        command(&state, &mut cx, add(11)).unwrap_err().code,
        "pull_request_not_in_diff",
        "a line outside every hunk is refused before GitHub would refuse the review"
    );
    command(
        &state,
        &mut cx,
        edit(PullRequestReviewDraftEdit::SetBody {
            body: "Two things".into(),
        }),
    )
    .unwrap();

    // A restart: a fresh host over the same data.
    state
        .update(&mut cx, |state, _| state.close_store())
        .unwrap();
    let store = SessionStore::open_at(dir.root().clone()).unwrap();
    let sessions = store.load_index().unwrap();
    let draft = sessions[0].pull_request_reviews.clone();
    assert_eq!(draft.len(), 1);
    assert_eq!(
        (
            draft[0].head.as_str(),
            draft[0].body.as_str(),
            draft[0]
                .comments
                .iter()
                .map(|comment| comment.end_line)
                .collect::<Vec<_>>()
        ),
        (OLD, "Two things", vec![3, 8])
    );
    let state = cx.new_entity(TestClientState::new(store.clone()));
    state.update(&mut cx, |state, _| {
        state.pull_requests = PullRequestRuntime::new(api);
        state.sessions = sessions.into();
    });
    let submit = |head: &str| Command::RunPullRequestAction {
        session_id: "active".into(),
        key: key.clone(),
        action: PullRequestAction::SubmitReview {
            verdict: tcode_protocol::PullRequestReviewVerdict::Comment,
            head: head.into(),
        },
    };
    let reviews = || {
        model
            .lock()
            .unwrap()
            .1
            .iter()
            .filter(|(line, _)| line.starts_with("POST /repos/sample/project/pulls/1/reviews "))
            .map(|(_, body)| serde_json::from_str::<Value>(body).unwrap())
            .collect::<Vec<_>>()
    };

    model.lock().unwrap().0 = NEW;
    assert_eq!(
        command(&state, &mut cx, submit(OLD)).unwrap(),
        CommandResponse::PullRequestAction(PullRequestActionResult::Rejected(
            tcode_protocol::PullRequestRejection::StaleHead { head: NEW.into() }
        ))
    );
    assert!(reviews().is_empty());
    assert_eq!(
        drafts(&state),
        draft,
        "a stale submission keeps the draft whole"
    );
    assert_eq!(
        command(
            &state,
            &mut cx,
            edit(PullRequestReviewDraftEdit::AddComment {
                head: NEW.into(),
                revision: NEW.into(),
                path: "src/lib.rs".into(),
                side: tcode_core::session::ReviewSide::New,
                start_line: 5,
                end_line: 5,
                body: "Read at the new head".into(),
            })
        )
        .unwrap_err()
        .code,
        "pull_request_head_changed",
        "a comment read at the new head is refused while the draft is at the old one"
    );
    assert_eq!(drafts(&state), draft);

    command(
        &state,
        &mut cx,
        edit(PullRequestReviewDraftEdit::MoveToHead),
    )
    .unwrap();
    let moved = drafts(&state);
    assert_eq!(moved[0].head, NEW);
    assert_eq!(
        moved[0]
            .comments
            .iter()
            .map(|comment| (comment.end_line, comment.placed, comment.revision.as_str()))
            .collect::<Vec<_>>(),
        vec![(3, true, NEW), (8, false, OLD)],
        "a comment whose line reads the same moves; one whose line changed is kept unplaced"
    );
    assert_eq!(
        command(&state, &mut cx, submit(NEW)).unwrap(),
        CommandResponse::PullRequestAction(PullRequestActionResult::Rejected(
            tcode_protocol::PullRequestRejection::Invalid
        )),
        "an unplaced comment is never sent"
    );
    let unplaced = moved[0].comments[1].id;
    command(
        &state,
        &mut cx,
        edit(PullRequestReviewDraftEdit::RemoveComment { id: unplaced }),
    )
    .unwrap();
    assert_eq!(
        command(&state, &mut cx, submit(NEW)).unwrap(),
        CommandResponse::PullRequestAction(PullRequestActionResult::Applied)
    );
    assert_eq!(
        reviews(),
        vec![json!({
            "commit_id": NEW,
            "event": "COMMENT",
            "body": "Two things",
            "comments": [{"path": "src/lib.rs", "line": 3, "side": "RIGHT", "body": "Line 3"}],
        })]
    );
    assert!(
        drafts(&state).is_empty(),
        "a submitted review leaves the draft"
    );
    cx.run_until(|state| !state.pull_requests.syncing && state.pull_requests.requested.is_empty());
    assert!(
        model
            .lock()
            .unwrap()
            .1
            .iter()
            .any(|(_, body)| body.contains("PullRequestSummaries")),
        "the sync reads the pull request after the write"
    );
    state
        .update(&mut cx, |state, _| state.close_store())
        .unwrap();
}

#[test]
fn a_review_draft_is_its_accounts_and_an_unanswered_submission_waits_for_a_later_read() {
    const HEAD: &str = "1111111111111111111111111111111111111111";
    let dir = TestStore::new("tcode-pr-review-account");
    let fixture = fixture::Fixture::new();
    let (first_account, second_account) =
        (client(&dir, &fixture), client_as(&dir, &fixture, "other"));
    let posted = Arc::new(Mutex::new(0));
    let counting = posted.clone();
    let _server = fixture.serve(move |exchange| {
        let line = exchange.request.lines().next().unwrap().to_owned();
        let body = String::from_utf8_lossy(&exchange.body).into_owned();
        if line.starts_with("POST /repos/sample/project/pulls/1/reviews ") {
            *counting.lock().unwrap() += 1;
            return exchange.reply(502, "", b"");
        }
        let reply = if line.starts_with("GET /repos/sample/project/pulls/1 ") {
            json!({"base": {"sha": HEAD}, "head": {"sha": HEAD}, "changed_files": 1, "node_id": "PR_1"})
        } else if body.contains("PullRequestSummaries") {
            json!({"data": {"s0": {"pullRequest": pr(1, "OPEN", false)}}})
        } else if body.contains("PullRequestConversation") {
            json!({"data": {"repository": {"pullRequest": {
                "id": "PR_1", "body": "Description", "createdAt": "2026-10-01T00:00:00Z",
                "url": "https://github.com/sample/project/pull/1", "author": {"login": "octocat"},
                "comments": {"pageInfo": {"hasNextPage": false}, "nodes": []},
                "reviews": {"pageInfo": {"hasNextPage": false}, "nodes": []},
            }}}})
        } else {
            json!({"data": {"repository": {"pullRequest": {"reviewThreads": {
                "pageInfo": {"hasNextPage": false}, "nodes": [],
            }}}}})
        };
        exchange.reply(200, "", &serde_json::to_vec(&reply).unwrap());
    });
    let key = PullRequestKey::new("github.com", "sample/project", 1);
    let mut cx = TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*dir).clone()));
    state.update(&mut cx, |state, _| {
        state.pull_requests = PullRequestRuntime::new(first_account);
        let meta = linked("active", 1, false);
        dir.upsert_meta(&meta).unwrap();
        state.sessions.push(meta);
    });
    let mut next_id = 0;
    let mut answer = |state: &TestEntity, cx: &mut TestAppContext, payload| {
        next_id += 1;
        state.deliver(cx, next_id, payload);
        let answer_id = next_id;
        cx.run_until_sent(|message| match message {
            tcode_protocol::HostMessage::Ack { id, result } if *id == answer_id => {
                Some(result.clone().map(|response| json!(response)))
            }
            tcode_protocol::HostMessage::QueryResult { id, result } if *id == answer_id => {
                Some(result.clone().map(|response| json!(response)))
            }
            _ => None,
        })
        .0
    };
    let body = |body: &str| {
        tcode_protocol::ClientPayload::Command(Command::EditPullRequestReviewDraft {
            session_id: "active".into(),
            key: key.clone(),
            edit: PullRequestReviewDraftEdit::SetBody { body: body.into() },
        })
    };
    let submit = tcode_protocol::ClientPayload::Command(Command::RunPullRequestAction {
        session_id: "active".into(),
        key: key.clone(),
        action: PullRequestAction::SubmitReview {
            verdict: tcode_protocol::PullRequestReviewVerdict::Comment,
            head: HEAD.into(),
        },
    });
    let conversation = tcode_protocol::ClientPayload::Query(tcode_protocol::Query::PullRequest {
        session_id: "active".into(),
        key: key.clone(),
        read: tcode_protocol::PullRequestRead::Conversation,
    });
    let drafts = |state: &TestEntity| {
        state.read(|state| state.find_meta("active").unwrap().pull_request_reviews)
    };

    answer(&state, &mut cx, body("Under the first account")).unwrap();
    assert_eq!(
        answer(&state, &mut cx, submit.clone()).unwrap(),
        json!(CommandResponse::PullRequestAction(
            PullRequestActionResult::Uncertain
        ))
    );
    assert!(drafts(&state)[0].uncertain, "the draft waits for a read");
    answer(&state, &mut cx, conversation.clone()).unwrap();
    assert!(
        !drafts(&state)[0].uncertain,
        "a conversation read after the submission shows what GitHub made of it"
    );
    let first = drafts(&state);

    // The host now reads GitHub as someone else.
    state.update(&mut cx, |state, _| {
        state.pull_requests = PullRequestRuntime::new(second_account);
    });
    let read = answer(&state, &mut cx, conversation).unwrap();
    let account = read["content"]["response"]["content"]["account"]
        .as_str()
        .unwrap();
    assert_ne!(account, first[0].account);
    assert_eq!(
        pull_request::review_draft(&drafts(&state), &key, account),
        None,
        "the other account's draft is not shown"
    );
    assert_eq!(
        answer(&state, &mut cx, submit).unwrap(),
        json!(CommandResponse::PullRequestAction(
            PullRequestActionResult::Rejected(tcode_protocol::PullRequestRejection::Invalid)
        )),
        "nor submitted"
    );
    assert_eq!(*posted.lock().unwrap(), 1);
    answer(&state, &mut cx, body("Under the second account")).unwrap();
    assert_eq!(drafts(&state)[0], first[0], "and kept as it was");
    assert_eq!(
        pull_request::review_draft(&drafts(&state), &key, account).map(|draft| &*draft.body),
        Some("Under the second account")
    );

    state.update(&mut cx, |state, cx| {
        state.unlink_pull_request("active", &key, cx)
    });
    assert_eq!(drafts(&state).len(), 2, "unlinking keeps both drafts");
    state
        .update(&mut cx, |state, _| state.close_store())
        .unwrap();
}

#[test]
fn a_stack_layer_is_never_merged_alone_and_a_revert_is_linked_as_opened() {
    const HEAD: &str = "2222222222222222222222222222222222222222";
    let dir = TestStore::new("tcode-pr-lifecycle");
    let fixture = fixture::Fixture::new();
    let api = client(&dir, &fixture);
    let operations = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = operations.clone();
    let _server = fixture.serve(move |exchange| {
        let sent: Value = serde_json::from_slice(&exchange.body).unwrap_or_default();
        let operation = sent["query"]
            .as_str()
            .and_then(|query| query.split(['(', ' ']).nth(1))
            .unwrap_or_default()
            .to_owned();
        seen.lock().unwrap().push(operation.clone());
        let rest = exchange
            .request
            .starts_with("GET /repos/sample/project/pulls/1 ");
        let reply = match operation.as_str() {
            _ if rest => json!({"base": {"sha": HEAD}, "head": {"sha": HEAD}, "node_id": "PR_1"}),
            "PullRequestActionState" => json!({"data": {"repository": {
                "viewerPermission": "WRITE", "mergeCommitAllowed": true,
                "pullRequest": {"id": "PR_1", "headRefOid": HEAD, "mergeStateStatus": "CLEAN"},
            }}}),
            "RevertPullRequest" => json!({"data": {"revertPullRequest": {"revertPullRequest": {
                "number": 9, "url": "https://github.com/sample/project/pull/9",
            }}}}),
            "MergePullRequest" => {
                json!({"data": {"mergePullRequest": {"pullRequest": {"merged": true}}}})
            }
            _ => json!({"data": {"s0": {"pullRequest": pr(1, "OPEN", false)}}}),
        };
        exchange.reply(200, "", &serde_json::to_vec(&reply).unwrap());
    });
    let key = PullRequestKey::new("github.com", "sample/project", 1);
    let mut cx = TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*dir).clone()));
    state.update(&mut cx, |state, _| {
        state.pull_requests = PullRequestRuntime::new(api);
        let meta = linked("active", 1, false);
        dir.upsert_meta(&meta).unwrap();
        state.sessions.push(meta);
    });
    let mut next_id = 0;
    let mut act = |state: &TestEntity, cx: &mut TestAppContext, action: PullRequestAction| {
        next_id += 1;
        let command = Command::RunPullRequestAction {
            session_id: "active".into(),
            key: key.clone(),
            action,
        };
        acked(state, cx, next_id, command).unwrap()
    };
    let merge = || PullRequestAction::Merge {
        head: HEAD.into(),
        method: tcode_core::pull_request::PullRequestMergeMethod::Merge,
        auto: false,
        remove_credits: false,
    };
    let update = || PullRequestAction::UpdateBranch {
        head: HEAD.into(),
        rebase: false,
    };
    let rejected = |rejection| {
        CommandResponse::PullRequestAction(PullRequestActionResult::Rejected(rejection))
    };
    let set_stack = |state: &TestEntity, cx: &mut TestAppContext, stack| {
        state.update(cx, |state, _| {
            state
                .sessions
                .iter_mut()
                .find(|meta| meta.id == "active")
                .unwrap()
                .pull_requests[0]
                .stack = stack;
        });
    };

    for action in [merge(), update()] {
        assert_eq!(
            act(&state, &mut cx, action),
            rejected(tcode_protocol::PullRequestRejection::StackUnknown),
            "a pull request whose stack is not known yet is not merged as one"
        );
    }
    let layer = |number: u64| tcode_core::pull_request::PullRequestStackLayer {
        url: format!("https://github.com/sample/project/pull/{number}"),
        number,
        head_branch: format!("layer-{number}"),
        state: PullRequestState::Open,
    };
    set_stack(
        &state,
        &mut cx,
        PullRequestStackState::Native(PullRequestStack {
            id: "stack".into(),
            number: 7,
            url: "https://github.com/sample/project/stacks/7".into(),
            base: "main".into(),
            layers: vec![layer(3), layer(1)],
        }),
    );
    for action in [merge(), update()] {
        assert_eq!(
            act(&state, &mut cx, action),
            rejected(tcode_protocol::PullRequestRejection::Invalid)
        );
    }
    assert!(
        operations.lock().unwrap().is_empty(),
        "a stack-routed write reads and sends nothing"
    );

    set_stack(&state, &mut cx, PullRequestStackState::None);
    assert_eq!(
        act(&state, &mut cx, merge()),
        CommandResponse::PullRequestAction(PullRequestActionResult::Applied)
    );
    assert_eq!(
        act(&state, &mut cx, PullRequestAction::Revert),
        CommandResponse::PullRequestAction(PullRequestActionResult::Opened {
            number: 9,
            url: "https://github.com/sample/project/pull/9".into()
        })
    );
    let links = state.read(|state| state.find_meta("active").unwrap().pull_requests);
    let revert = links
        .iter()
        .find(|link| link.key.number == 9)
        .expect("the revert is linked to the thread");
    assert_eq!(
        (revert.source, revert.url.as_str()),
        (
            PullRequestSource::Opened,
            "https://github.com/sample/project/pull/9"
        )
    );
    // The sync reads the pull requests after the writes.
    cx.run_until(|_| {
        operations
            .lock()
            .unwrap()
            .iter()
            .any(|operation| operation == "PullRequestSummaries")
    });
    state
        .update(&mut cx, |state, _| state.close_store())
        .unwrap();
}

/// GitHub as a stack merge meets it: stack #50 of #2 and #3 into `main`, an asynchronous merge
/// of #3 answering by operation, and the summaries the sync reads.
#[derive(Default)]
struct StackHost {
    submissions: usize,
    mutations: usize,
    asked: Vec<String>,
    status: HashMap<String, &'static str>,
    state: &'static str,
}

fn stack_reply(host: &Mutex<StackHost>, exchange: fixture::Exchange) {
    let line = exchange
        .request
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned();
    let mut host = host.lock().unwrap();
    let path = line.split(' ').nth(1).unwrap_or_default().to_owned();
    let reply = if path.starts_with("/repos/sample/project/stacks?pull_request=")
        || path == "/repos/sample/project/stacks/50"
    {
        let layers: Vec<_> = [2, 3]
            .map(|number| {
                json!({
                    "number": number, "title": format!("Change {number}"), "state": "open",
                    "draft": false,
                    "head": {"ref": format!("layer-{number}"), "sha": format!("{number}").repeat(40)},
                })
            })
            .to_vec();
        let stack = json!({
            "number": 50, "url": "https://api.github.com/repos/sample/project/stacks/50",
            "base": {"ref": "main"},
            "pull_requests": layers,
        });
        if path.ends_with("/50") {
            stack
        } else {
            json!([stack])
        }
    } else if line.starts_with("PUT /repos/sample/project/pulls/3/merge-async ") {
        host.submissions += 1;
        json!({"status": "pending", "details": {"uuid": "op-1"}})
    } else if let Some(id) = path.strip_prefix("/repos/sample/project/pulls/3/merge-async/") {
        host.asked.push(id.to_owned());
        json!({"status": host.status.get(id).copied().unwrap_or("pending"), "details": {"uuid": id}})
    } else {
        let sent: Value = serde_json::from_slice(&exchange.body).unwrap_or_default();
        if sent["query"]
            .as_str()
            .is_some_and(|query| query.trim_start().starts_with("mutation"))
        {
            host.mutations += 1;
        }
        let mut data = serde_json::Map::new();
        for (name, number) in sent["variables"].as_object().into_iter().flatten() {
            if let Some(alias) = name.strip_suffix("_number") {
                let mut row = pr(number.as_u64().unwrap(), host.state, true);
                row["stack"] = json!({"number": 50});
                data.insert(alias.to_owned(), json!({"pullRequest": row}));
            }
        }
        json!({"data": data})
    };
    exchange.reply(200, "", &serde_json::to_vec(&reply).unwrap());
}

/// A thread linking #3 and the layer below it, #2, of stack #50 into `main`.
fn stack_thread() -> SessionMeta {
    let stack = PullRequestStackState::Native(PullRequestStack {
        id: "50".into(),
        number: 50,
        url: "https://github.com/sample/project/stacks/50".into(),
        base: "main".into(),
        layers: [2, 3]
            .map(|number| tcode_core::pull_request::PullRequestStackLayer {
                url: format!("https://github.com/sample/project/pull/{number}"),
                number,
                head_branch: format!("layer-{number}"),
                state: PullRequestState::Open,
            })
            .to_vec(),
    });
    let mut meta = linked("active", 3, false);
    pull_request::link_pull_request(
        &mut meta.pull_requests,
        PullRequestKey::new("github.com", "sample/project", 2),
        "https://github.com/sample/project/pull/2".into(),
        PullRequestSource::Stack,
        1,
        false,
    );
    for link in &mut meta.pull_requests {
        link.stack = stack.clone();
    }
    meta
}

#[test]
fn a_stack_merge_is_followed_across_a_restart_and_never_submitted_again() {
    use tcode_core::pull_request::{PullRequestStackOperation, StackOperationKind};
    use tcode_protocol::{
        EventEnvelope, HostMessage, PullRequestRejection, PullRequestStackHead,
        RuntimeNotification, RuntimeToast, ServerEvent,
    };
    let dir = TestStore::new("tcode-pr-stack-merge");
    let fixture = fixture::Fixture::new();
    let api = client(&dir, &fixture);
    let host = Arc::new(Mutex::new(StackHost {
        state: "OPEN",
        ..StackHost::default()
    }));
    let serving = host.clone();
    let _server = fixture.serve(move |exchange| stack_reply(&serving, exchange));
    let meta = stack_thread();
    let mut cx = TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*dir).clone()));
    state.update(&mut cx, |state, _| {
        state.pull_requests = PullRequestRuntime::new(api);
        dir.upsert_meta(&meta).unwrap();
        state.sessions.push(meta);
    });
    let target = PullRequestKey::new("github.com", "sample/project", 3);
    let merge = PullRequestAction::MergeStack {
        stack: 50,
        heads: [2, 3]
            .map(|number| PullRequestStackHead {
                number,
                head: format!("{number}").repeat(40),
            })
            .to_vec(),
        method: tcode_core::pull_request::PullRequestMergeMethod::Merge,
    };
    let mut next_id = 0;
    let mut act = |cx: &mut TestAppContext| {
        next_id += 1;
        let command = Command::RunPullRequestAction {
            session_id: "active".into(),
            key: target.clone(),
            action: merge.clone(),
        };
        acked(&state, cx, next_id, command).unwrap()
    };
    let operation = |state: &TestEntity| {
        state.read(|state| {
            state
                .find_meta("active")
                .unwrap()
                .pull_request_operations
                .first()
                .map(|operation| operation.kind.clone())
        })
    };
    let ended = |cx: &mut TestAppContext| {
        cx.drain_outgoing()
            .into_iter()
            .filter_map(|message| match message {
                HostMessage::Event(EventEnvelope {
                    event:
                        ServerEvent::Runtime(RuntimeNotification::Toast(
                            RuntimeToast::PullRequestStack {
                                result,
                                late,
                                layers,
                                ..
                            },
                        )),
                    ..
                }) => Some((result, late, layers)),
                _ => None,
            })
            .collect::<Vec<_>>()
    };

    // Submitted once, then followed by its operation until GitHub says it merged. GitHub keeps
    // answering pending until the record is read, so no poll can end it first.
    assert_eq!(
        act(&mut cx),
        CommandResponse::PullRequestAction(PullRequestActionResult::Pending {
            id: "op-1".into(),
            adopted: false,
        })
    );
    assert!(matches!(
        operation(&state),
        Some(StackOperationKind::Merging { ref id, target: 3, ref layers, adopted: false })
            if id == "op-1" && *layers == [2, 3]
    ));
    host.lock().unwrap().status.insert("op-1".into(), "merged");
    cx.run_until(|state| {
        state
            .find_meta("active")
            .unwrap()
            .pull_request_operations
            .is_empty()
    });
    assert_eq!(
        ended(&mut cx),
        vec![(PullRequestActionResult::Applied, false, vec![2, 3])]
    );

    let seed = |state: &TestEntity, cx: &mut TestAppContext, kind| {
        state.update(cx, |state, cx| {
            let mut meta = state.find_meta("active").unwrap();
            meta.pull_request_operations = vec![PullRequestStackOperation {
                host: "github.com".into(),
                repository: "sample/project".into(),
                stack: 50,
                started_at: now_secs() - 400,
                kind,
            }];
            state.save_pull_request_meta(meta, cx);
        })
    };
    // Until a sync reads what became of an unconfirmed merge, the stack takes no other write.
    seed(
        &state,
        &mut cx,
        StackOperationKind::MergeUnconfirmed {
            id: "op-0".into(),
            target: 3,
            layers: vec![2, 3],
            checked: false,
        },
    );
    assert_eq!(
        act(&mut cx),
        CommandResponse::PullRequestAction(PullRequestActionResult::Rejected(
            PullRequestRejection::OperationRunning
        ))
    );

    // A host restarted after the five minutes asks once more, finds it still pending, and
    // keeps it as unconfirmed instead of submitting it again; the sync that then reads the
    // target merged ends it, reported as finished late.
    host.lock().unwrap().state = "MERGED";
    seed(
        &state,
        &mut cx,
        StackOperationKind::Merging {
            id: "op-2".into(),
            target: 3,
            layers: vec![2, 3],
            adopted: false,
        },
    );
    state.update(&mut cx, |state, cx| state.resume_stack_operations(cx));
    cx.run_until(|state| {
        state
            .find_meta("active")
            .unwrap()
            .pull_request_operations
            .is_empty()
    });
    assert_eq!(
        ended(&mut cx),
        vec![
            (
                PullRequestActionResult::MergeUnconfirmed { id: "op-2".into() },
                false,
                vec![2, 3]
            ),
            (PullRequestActionResult::Applied, true, vec![2, 3]),
        ]
    );
    let host = host.lock().unwrap();
    assert_eq!(host.submissions, 1, "nothing is submitted again");
    assert_eq!(host.asked.iter().filter(|id| *id == "op-2").count(), 1);
    assert_eq!(
        host.mutations, 0,
        "a stack merge is never one pull request's merge"
    );
    drop(host);
    state
        .update(&mut cx, |state, _| state.close_store())
        .unwrap();
}

#[test]
fn an_ended_rebase_is_kept_for_every_device_until_the_next_full_sync_and_blocks_no_write() {
    use tcode_core::pull_request::{
        PullRequestStackOperation, StackOperationKind, StackRebaseFailure, StackRebaseLayer,
        StackRebaseStep,
    };
    use tcode_protocol::PullRequestStackHead;
    let dir = TestStore::new("tcode-pr-stack-rebase-ended");
    let fixture = fixture::Fixture::new();
    let api = client(&dir, &fixture);
    let host = Arc::new(Mutex::new(StackHost {
        state: "OPEN",
        ..StackHost::default()
    }));
    let serving = host.clone();
    let _server = fixture.serve(move |exchange| stack_reply(&serving, exchange));
    let mut meta = stack_thread();
    let ended = StackOperationKind::RebaseEnded {
        layers: vec![
            StackRebaseLayer {
                number: 2,
                branch: "layer-2".into(),
                step: StackRebaseStep::Pushed {
                    from: "2".repeat(40),
                    to: "4".repeat(40),
                },
            },
            StackRebaseLayer {
                number: 3,
                branch: "layer-3".into(),
                step: StackRebaseStep::Failed {
                    reason: StackRebaseFailure::Conflict,
                },
            },
        ],
    };
    meta.pull_request_operations = vec![PullRequestStackOperation {
        host: "github.com".into(),
        repository: "sample/project".into(),
        stack: 50,
        started_at: now_secs(),
        kind: ended.clone(),
    }];
    let mut cx = TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*dir).clone()));
    state.update(&mut cx, |state, _| {
        state.pull_requests = PullRequestRuntime::new(api);
        dir.upsert_meta(&meta).unwrap();
        state.sessions.push(meta);
    });
    let operation = |state: &TestEntity| {
        state.read(|state| {
            state
                .find_meta("active")
                .unwrap()
                .pull_request_operations
                .first()
                .map(|operation| operation.kind.clone())
        })
    };

    // The reads its end asks for refresh the layers and keep how it ended.
    state.update(&mut cx, |state, cx| {
        state.request_pull_request_sync(PullRequestKey::new("github.com", "sample/project", 3), cx)
    });
    cx.run_until(|state| {
        !state.pull_requests.syncing
            && !state.pull_requests.sync_scheduled
            && state.pull_requests.requested.is_empty()
    });
    assert_eq!(operation(&state), Some(ended.clone()));

    // A stopped rebase is no running write: the stack takes the next one.
    host.lock().unwrap().status.insert("op-1".into(), "merged");
    let merge = Command::RunPullRequestAction {
        session_id: "active".into(),
        key: PullRequestKey::new("github.com", "sample/project", 3),
        action: PullRequestAction::MergeStack {
            stack: 50,
            heads: [2, 3]
                .map(|number| PullRequestStackHead {
                    number,
                    head: format!("{number}").repeat(40),
                })
                .to_vec(),
            method: tcode_core::pull_request::PullRequestMergeMethod::Merge,
        },
    };
    assert_eq!(
        acked(&state, &mut cx, 1, merge).unwrap(),
        CommandResponse::PullRequestAction(PullRequestActionResult::Pending {
            id: "op-1".into(),
            adopted: false,
        })
    );
    cx.run_until(|state| {
        state
            .find_meta("active")
            .unwrap()
            .pull_request_operations
            .is_empty()
    });

    // The next full sync drops it.
    state.update(&mut cx, |state, cx| {
        let mut meta = state.find_meta("active").unwrap();
        meta.pull_request_operations = vec![PullRequestStackOperation {
            host: "github.com".into(),
            repository: "sample/project".into(),
            stack: 50,
            started_at: now_secs(),
            kind: ended,
        }];
        state.save_pull_request_meta(meta, cx);
    });
    sweep(&state, &mut cx);
    assert_eq!(operation(&state), None);
    state
        .update(&mut cx, |state, _| state.close_store())
        .unwrap();
}

#[test]
fn a_partly_posted_review_keeps_only_unposted_drafts_for_the_next_submission() {
    use std::collections::BTreeMap;
    use tcode_core::{
        pull_request::{HostTerms, PullRequestReviewDraftComment},
        pull_request_watch::{PullRequestRemark, PullRequestWatchRead},
        session::ReviewSide,
        settings::{HostSettings, HostStatus},
    };
    use tcode_protocol::{
        PullRequestRead, PullRequestReadResponse, PullRequestRejection, PullRequestReviewVerdict,
    };
    use tcode_services::forge::{Anchoring, Moved, Repository, Tails};

    struct ReviewHost {
        first: Mutex<Option<PullRequestActionResult>>,
        sent: Mutex<Vec<(String, Vec<u64>)>>,
    }
    impl Forge for ReviewHost {
        fn terms(&self, _: &PullRequestKey) -> &'static HostTerms {
            &pull_request::GITLAB
        }
        fn capabilities(&self, _: &PullRequestKey) -> tcode_protocol::PullRequestCapabilities {
            unreachable!()
        }
        fn configure(&self, _: BTreeMap<String, HostSettings>) {
            unreachable!()
        }
        fn credential_status(&self) -> BTreeMap<String, HostStatus> {
            unreachable!()
        }
        fn forget_credential(&self, _: &str) {
            unreachable!()
        }
        fn pull_request_url(&self, _: &str) -> Option<(PullRequestKey, String)> {
            unreachable!()
        }
        fn checkout_repository(
            &self,
            _: &tcode_services::forge::Checkout<'_>,
        ) -> Option<Repository> {
            unreachable!()
        }
        fn repository(&self, _: &str, _: &str) -> Option<Repository> {
            unreachable!()
        }
        fn url(&self, _: &PullRequestKey) -> Option<String> {
            unreachable!()
        }
        fn summary(&self, _: &PullRequestKey) -> Result<Summary, ForgeError> {
            Ok(Summary {
                snapshot: PullRequestSnapshot {
                    state: PullRequestState::Open,
                    title: "Review fixture".into(),
                    head_branch: "feature".into(),
                    base_branch: "main".into(),
                    is_draft: false,
                    updated_at: "2026-10-01T00:00:00Z".into(),
                    synced_at: now_secs(),
                    closed_at: None,
                    merged_at: None,
                    author: None,
                    additions: None,
                    deletions: None,
                    changed_files: None,
                    review_decision: None,
                    checks_state: None,
                    mergeability: pull_request::Mergeability::Clean,
                },
                stack_number: None,
            })
        }
        fn stack(&self, _: &PullRequestKey) -> Result<PullRequestStackState, ForgeError> {
            Ok(PullRequestStackState::None)
        }
        fn read(
            &self,
            _: &PullRequestKey,
            _: PullRequestRead,
        ) -> Result<(PullRequestReadResponse, SystemTime), ForgeError> {
            unreachable!()
        }
        fn invalidate(&self, _: &PullRequestKey) {}
        fn account(&self, _: &PullRequestKey) -> Result<String, ForgeError> {
            Ok("reviewer".into())
        }
        fn set_viewed(&self, _: &PullRequestKey, _: &[String], _: bool) -> Result<(), ForgeError> {
            unreachable!()
        }
        fn act(&self, _: &PullRequestKey, _: &PullRequestAction) -> PullRequestActionResult {
            unreachable!()
        }
        fn submit_review(
            &self,
            _: &PullRequestKey,
            _: PullRequestReviewVerdict,
            _: &str,
            body: &str,
            comments: &[PullRequestReviewDraftComment],
        ) -> PullRequestActionResult {
            self.sent.lock().unwrap().push((
                body.into(),
                comments.iter().map(|comment| comment.id).collect(),
            ));
            self.first
                .lock()
                .unwrap()
                .take()
                .unwrap_or(PullRequestActionResult::Applied)
        }
        fn commentable(
            &self,
            _: &PullRequestKey,
            _: &str,
            _: &str,
            _: ReviewSide,
            _: (u32, u32),
        ) -> Result<Anchoring, ForgeError> {
            Ok(Anchoring::InDiff)
        }
        fn reanchor(
            &self,
            _: &PullRequestKey,
            _: &[PullRequestReviewDraftComment],
        ) -> Result<(String, Vec<Moved>), ForgeError> {
            unreachable!()
        }
        fn watch_detail(&self, _: &PullRequestKey) -> Result<PullRequestWatchRead, ForgeError> {
            unreachable!()
        }
        fn activity(
            &self,
            _: &PullRequestKey,
            _: &mut Tails,
        ) -> Result<Option<Vec<PullRequestRemark>>, ForgeError> {
            unreachable!()
        }
    }

    // Receipts use path:line, so two comments at the same location must consume two receipts.
    for (applied, failure, remaining, body, uncertain) in [
        (
            vec!["src/lib.rs:3"],
            PullRequestActionResult::Rejected(PullRequestRejection::Failed),
            vec![2, 3],
            "Summary",
            false,
        ),
        (
            vec!["src/lib.rs:3", "src/lib.rs:3"],
            PullRequestActionResult::Uncertain,
            vec![3],
            "Summary",
            true,
        ),
        (
            vec!["src/lib.rs:3", "src/lib.rs:3", "src/lib.rs:8", "summary"],
            PullRequestActionResult::Rejected(PullRequestRejection::Failed),
            vec![],
            "",
            false,
        ),
    ] {
        let dir = TestStore::new("tcode-pr-review-partial");
        let receipt = PullRequestActionResult::Partial {
            applied: applied.iter().map(|name| (*name).into()).collect(),
            unapplied: if body.is_empty() {
                vec!["approval".into()]
            } else {
                let mut names = vec!["src/lib.rs:8".into(), "summary".into(), "approval".into()];
                if applied.len() == 1 {
                    names.insert(0, "src/lib.rs:3".into());
                }
                names
            },
            failure: Box::new(failure),
        };
        let forge = Arc::new(ReviewHost {
            first: Mutex::new(Some(receipt.clone())),
            sent: Mutex::default(),
        });
        let mut cx = TestAppContext::default();
        let state = cx.new_entity(TestClientState::new((*dir).clone()));
        let meta = linked("active", 1, false);
        let key = meta.pull_requests[0].key.clone();
        state.update(&mut cx, |state, _| {
            state.pull_requests = PullRequestRuntime::new(forge.clone());
            dir.upsert_meta(&meta).unwrap();
            state.sessions.push(meta);
        });
        let mut id = 0;
        let mut command = |command| {
            id += 1;
            acked(&state, &mut cx, id, command).unwrap()
        };
        let edit = |edit| Command::EditPullRequestReviewDraft {
            session_id: "active".into(),
            key: key.clone(),
            edit,
        };
        command(edit(PullRequestReviewDraftEdit::SetBody {
            body: "Summary".into(),
        }));
        for line in [3, 3, 8] {
            command(edit(PullRequestReviewDraftEdit::AddComment {
                head: "head".into(),
                revision: "head".into(),
                path: "src/lib.rs".into(),
                side: ReviewSide::New,
                start_line: line,
                end_line: line,
                body: format!("Comment at {line}"),
            }));
        }
        let submit = || Command::RunPullRequestAction {
            session_id: "active".into(),
            key: key.clone(),
            action: PullRequestAction::SubmitReview {
                verdict: PullRequestReviewVerdict::Approve,
                head: "head".into(),
            },
        };
        assert_eq!(
            command(submit()),
            CommandResponse::PullRequestAction(receipt)
        );
        let drafts = state.read(|state| state.find_meta("active").unwrap().pull_request_reviews);
        if body.is_empty() {
            assert!(
                drafts.is_empty(),
                "all draft content was confirmed posted even though approval failed"
            );
        } else {
            assert_eq!(drafts[0].body, body);
            assert_eq!(
                drafts[0]
                    .comments
                    .iter()
                    .map(|comment| comment.id)
                    .collect::<Vec<_>>(),
                remaining
            );
            assert_eq!(drafts[0].uncertain, uncertain);
        }
        if !uncertain {
            assert_eq!(
                command(submit()),
                CommandResponse::PullRequestAction(PullRequestActionResult::Applied)
            );
            assert_eq!(
                *forge.sent.lock().unwrap(),
                vec![("Summary".into(), vec![1, 2, 3]), (body.into(), remaining)]
            );
        }
        cx.run_until(|state| {
            !state.pull_requests.syncing && state.pull_requests.requested.is_empty()
        });
        state
            .update(&mut cx, |state, _| state.close_store())
            .unwrap();
        let reopened = SessionStore::open_at(dir.root().clone()).unwrap();
        let persisted = reopened.load_index().unwrap()[0]
            .pull_request_reviews
            .clone();
        if uncertain {
            assert_eq!(
                persisted, drafts,
                "the remaining uncertain draft survives a restart"
            );
        } else {
            assert!(persisted.is_empty());
        }
    }
}
