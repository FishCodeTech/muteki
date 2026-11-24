use super::*;
use crate::app::test_support::*;
use crate::app::{ProviderLauncher, active_session::Runtime};
use control_mcp::{ControlOp, MoveThreadParams, ReferenceParams};
use tcode_core::control::{ControlScope, ControlTier, ReferenceOrigin};

/// Providers that never come up, so what a call queued stays observable.
fn idle_launcher() -> ProviderLauncher {
    ProviderLauncher(Arc::new(|_, _| Box::pin(std::future::pending())))
}

fn host(store: &TestStore, cx: &mut TestAppContext) -> TestEntity {
    let mut state = TestClientState::new((**store).clone());
    state.set_provider_launcher_for_test(idle_launcher());
    cx.new_entity(state)
}

fn call(
    state: &mut AppState,
    cx: &mut HostCx,
    caller: &str,
    op: ControlOp,
) -> Result<serde_json::Value, String> {
    let (reply, response) = smol::channel::bounded(1);
    state.handle_control_op(caller.into(), op, reply, cx);
    response.try_recv().unwrap()
}

fn thread(id: &str, project: &str, provider: ProviderKind) -> SessionMeta {
    let mut meta = SessionMeta::new(provider, PathBuf::from("/tmp/synthetic-checkout"), None);
    meta.id = id.into();
    meta.title = format!("Thread {id}");
    meta.project_id = Some(project.into());
    meta
}

fn park(state: &mut AppState, session: ActiveSession) {
    state.sessions.push(session.meta.clone());
    state
        .residents
        .parked
        .insert(session.meta.id.clone(), session);
}

fn start(project_id: &str) -> ControlOp {
    ControlOp::StartThread(StartThreadParams {
        project_id: project_id.into(),
        cwd: None,
        workspace: None,
        provider: "codex".into(),
        model: None,
        profile: None,
        options: None,
        title: Some("Investigate the flaky test".into()),
        message: "Find out why the sync test flakes".into(),
        attachments: None,
        request_id: "start-1".into(),
    })
}

fn started(state: &AppState) -> Vec<SessionMeta> {
    state
        .sessions
        .iter()
        .filter(|meta| meta.started_by.is_some())
        .cloned()
        .collect()
}

/// `start_thread` makes one ordinary thread whose first message is the agent's, leaves the
/// user's standing draft alone, and a retry with the same request id — also after the host
/// restarts — answers with that thread instead of making another.
#[test]
fn start_thread_makes_one_ordinary_thread_across_retries_and_restarts() {
    let store = TestStore::new("tcode-control-start-thread");
    let root = store.root().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let cx = &mut TestAppContext::default();
    let state = host(&store, cx);
    let project_id = state.update(cx, |state, cx| {
        let project_id = state.create_project(root.clone(), cx).unwrap();
        let mut caller = thread("caller", &project_id, ProviderKind::ClaudeCode);
        caller.cwd = root.clone();
        state.persist_meta(&caller, cx);
        state.start_draft(project_id.clone(), root.clone(), cx);
        project_id
    });
    let draft = state.read(|state| state.active_session_id().unwrap().to_owned());

    // The model may not choose the permission mode, leave the project or attach a file the
    // user did not attach; nothing is made.
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("notes.txt"), b"notes").unwrap();
    let shot = state.read(|state| state.attachments_dir_for("caller").join("shot.png"));
    std::fs::create_dir_all(shot.parent().unwrap()).unwrap();
    std::fs::write(&shot, b"png").unwrap();
    let with = |edit: &dyn Fn(&mut StartThreadParams)| {
        let ControlOp::StartThread(mut params) = start(&project_id) else {
            unreachable!()
        };
        edit(&mut params);
        ControlOp::StartThread(params)
    };
    let outside = std::env::temp_dir().display().to_string();
    let notes = root.join("notes.txt").display().to_string();
    for (op, refusal) in [
        (
            with(&|p| {
                p.options = Some(
                    [("permissions".to_owned(), serde_json::json!("full_access"))]
                        .into_iter()
                        .collect(),
                )
            }),
            "refused: option permissions sets the permission mode, which only the user changes",
        ),
        (
            with(&|p| p.cwd = Some("../".into())),
            "refused: cwd must be inside the project",
        ),
        (
            with(&|p| p.cwd = Some(outside.clone())),
            "refused: cwd must be inside the project",
        ),
        (with(&|p| p.attachments = Some(vec![notes.clone()])), ""),
    ] {
        let error = state
            .update(cx, |state, cx| call(state, cx, "caller", op))
            .unwrap_err();
        if refusal.is_empty() {
            assert!(
                error.starts_with("refused: attachment ") && error.ends_with("in your scope"),
                "{error}"
            );
        } else {
            assert_eq!(error, refusal);
        }
    }
    state.read(|state| assert!(started(state).is_empty()));
    state.update(cx, |state, _| {
        let project = state
            .projects
            .iter_mut()
            .find(|project| project.id == project_id)
            .unwrap();
        project
            .permission_defaults
            .insert("codex".into(), "read_only".into());
    });

    let first = state
        .update(cx, |state, cx| {
            call(
                state,
                cx,
                "caller",
                with(&|p| {
                    p.cwd = Some("sub".into());
                    p.attachments = Some(vec![shot.display().to_string()]);
                }),
            )
        })
        .unwrap();
    assert_eq!(first["created"], true);
    let id = first["thread_id"].as_str().unwrap().to_owned();
    state.read(|state| {
        let threads = started(state);
        assert_eq!(threads.len(), 1);
        let meta = &threads[0];
        assert_eq!(meta.id, id);
        assert_eq!(
            meta.parent_session_id, None,
            "an ordinary thread, not a child"
        );
        assert_eq!(meta.title, "Investigate the flaky test");
        assert_eq!(meta.cwd, root.join("sub"));
        assert!(
            meta.option_selections
                .iter()
                .any(|selection| selection.id == "permissions"
                    && selection.value == serde_json::json!("read_only")),
            "the project's default permission: {:?}",
            meta.option_selections
        );
        let session = state.resident(&id).unwrap();
        assert!(!session.draft);
        assert_eq!(session.queue[0].text, "Find out why the sync test flakes");
        assert_eq!(session.queue[0].attachments.len(), 1);
        assert_eq!(session.queue[0].origin, MessageOrigin::Agent);

        assert_eq!(state.active_session_id(), Some(draft.as_str()));
        let standing = state.resident(&draft).unwrap();
        assert!(standing.draft, "the user's draft stays a draft");
        assert!(standing.queue.is_empty());
    });

    let retry = state
        .update(cx, |state, cx| {
            call(state, cx, "caller", start(&project_id))
        })
        .unwrap();
    assert_eq!(retry["created"], false);
    assert_eq!(retry["thread_id"], id.as_str());
    state.read(|state| assert_eq!(started(state).len(), 1));
    cx.run_until_parked();
    drop(state);

    let cx = &mut TestAppContext::default();
    let restarted = host(&store, cx);
    let after_restart = restarted
        .update(cx, |state, cx| {
            call(state, cx, "caller", start(&project_id))
        })
        .unwrap();
    assert_eq!(after_restart["created"], false);
    assert_eq!(after_restart["thread_id"], id.as_str());
    restarted.read(|state| assert_eq!(started(state).len(), 1));
}

/// An untitled `start_thread` thread is titled from its first message once the provider
/// accepts it, as a thread the user started is, though it runs in the background.
#[test]
fn an_untitled_started_thread_is_titled_from_its_first_message() {
    let store = TestStore::new("tcode-control-start-title");
    let root = store.root().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let cx = &mut TestAppContext::default();
    let provider = scripted_provider(ProviderKind::Codex);
    let (commands, events) = (provider.commands.clone(), provider.events.clone());
    let state = cx.new_entity({
        let mut state = TestClientState::new((*store).clone());
        state.set_provider_launcher_for_test(provider.launcher);
        state
    });
    let project_id = state.update(cx, |state, cx| {
        let project_id = state.create_project(root.clone(), cx).unwrap();
        let mut caller = thread("caller", &project_id, ProviderKind::ClaudeCode);
        caller.cwd = root.clone();
        state.persist_meta(&caller, cx);
        project_id
    });
    let mut untitled = start(&project_id);
    if let ControlOp::StartThread(params) = &mut untitled {
        params.title = None;
    }
    let id = state
        .update(cx, |state, cx| call(state, cx, "caller", untitled))
        .unwrap()["thread_id"]
        .as_str()
        .unwrap()
        .to_owned();
    cx.run_until(|_| !commands.is_empty());
    let Ok(SessionCommand::SendTurn { delivery_id, .. }) = commands.try_recv() else {
        panic!("expected the first message to reach the provider");
    };
    events
        .try_send(AgentEvent::TurnAccepted { delivery_id })
        .unwrap();
    cx.run_until(|state| {
        state
            .find_meta(&id)
            .is_some_and(|meta| !meta.title.starts_with("New "))
    });
    state.read(|state| {
        assert_eq!(
            state.find_meta(&id).unwrap().title,
            "Find out why the sync test flakes"
        );
    });
}

/// `send_message` reaches threads in each mode as the agent's message, reopens a settled
/// thread and says so, and never counts as the user's activity.
#[test]
fn send_message_queues_steers_and_schedules_as_the_agent() {
    let store = TestStore::new("tcode-control-send");
    let cx = &mut TestAppContext::default();
    let state = host(&store, cx);
    let (commands, steered) = smol::channel::unbounded();
    state.update(cx, |state, _| {
        state
            .sessions
            .push(thread("caller", "p", ProviderKind::ClaudeCode));
        let mut settled = thread("settled", "p", ProviderKind::Codex);
        settled.settled_at = Some(1);
        park(state, ActiveSession::new(settled, false, Vec::new()));
        let mut running = ActiveSession::new(
            thread("running", "p", ProviderKind::Codex),
            false,
            Vec::new(),
        );
        running.runtime = Runtime::Live(commands);
        running.turn_in_flight = true;
        park(state, running);
        park(
            state,
            ActiveSession::new(thread("later", "p", ProviderKind::Codex), false, Vec::new()),
        );
        state.sessions.push(thread("far", "q", ProviderKind::Codex));
    });
    let send = |ids: &[&str], mode: SendMode| {
        ControlOp::SendMessage(SendMessageParams {
            thread_ids: ids.iter().map(|id| id.to_string()).collect(),
            text: "Rebase on main when you can".into(),
            mode,
            attachments: None,
            request_id: None,
        })
    };
    let at = now_secs() + 3600;
    let hosts = ControlOp::SendMessage(SendMessageParams {
        attachments: Some(vec![std::env::temp_dir().display().to_string()]),
        ..match send(&["later"], SendMode::Queue) {
            ControlOp::SendMessage(params) => params,
            _ => unreachable!(),
        }
    });
    let refused = state
        .update(cx, |state, cx| call(state, cx, "caller", hosts))
        .unwrap_err();
    assert!(refused.starts_with("refused: attachment "), "{refused}");
    let (queued, steer, scheduled) = state.update(cx, |state, cx| {
        (
            call(state, cx, "caller", send(&["settled"], SendMode::Queue)).unwrap(),
            call(
                state,
                cx,
                "caller",
                send(&["running", "settled", "far"], SendMode::Steer),
            )
            .unwrap(),
            call(
                state,
                cx,
                "caller",
                send(&["later"], SendMode::Schedule { at }),
            )
            .unwrap(),
        )
    });
    cx.run_until_parked();

    assert_eq!(queued["results"][0]["outcome"], "applied");
    assert_eq!(queued["results"][0]["reason"], "queued");
    assert_eq!(
        queued["results"][0]["side_effects"][0],
        "unsettled: a message reopens a settled thread"
    );
    assert_eq!(steer["results"][0]["reason"], "steered");
    assert_eq!(steer["results"][1]["reason"], "queued");
    assert_eq!(
        steer["results"][1]["side_effects"][0],
        "no turn was running; queued instead"
    );
    assert_eq!(
        steer["results"][2],
        serde_json::json!({
            "target": "far",
            "outcome": "skipped",
            "reason": "outside_scope: your grant's scope is project",
        })
    );
    assert_eq!(scheduled["results"][0]["reason"], "scheduled");
    assert!(matches!(
        steered.try_recv(),
        Ok(SessionCommand::Steer { text, .. }) if text == "Rebase on main when you can"
    ));
    state.read(|state| {
        assert!(!state.find_meta("settled").unwrap().is_settled());
        let settled = state.resident("settled").unwrap();
        assert_eq!(settled.queue.len(), 2);
        assert!(
            settled
                .queue
                .iter()
                .all(|m| m.origin == MessageOrigin::Agent)
        );
        let running = state.resident("running").unwrap();
        assert_eq!(running.pending_steers[0].origin, Some(MessageOrigin::Agent));
        let later = state.resident("later").unwrap();
        assert_eq!(later.queue[0].origin, MessageOrigin::Agent);
        assert!(later.queue[0].not_before.is_some());
        for id in ["settled", "running", "later"] {
            assert_eq!(
                state
                    .thread_activity
                    .get(id)
                    .and_then(|activity| activity.last_human_message_at),
                None,
                "{id}: an agent's message is not the user's activity"
            );
        }
    });
}

/// Dropping a queued message the user wrote needs `history_edit`; the agent's own may go.
/// A fork answers with the new thread's id.
#[test]
fn control_thread_guards_human_queued_messages_and_forks() {
    let store = TestStore::new("tcode-control-thread");
    let cx = &mut TestAppContext::default();
    let state = host(&store, cx);
    let (human, agent) = state.update(cx, |state, _| {
        state
            .sessions
            .push(thread("caller", "p", ProviderKind::ClaudeCode));
        let mut queued = ActiveSession::new(
            thread("queued", "p", ProviderKind::Codex),
            false,
            Vec::new(),
        );
        let human = queued.push_queued("from the user".into(), Vec::new());
        let agent = queued.push_queued("from an agent".into(), Vec::new());
        queued.queue[1].origin = MessageOrigin::Agent;
        park(state, queued);
        let mut source = thread("source", "p", ProviderKind::Codex);
        source.resume_cursor = Some(agent::ResumeCursor(serde_json::json!({"thread_id": "t"})));
        source.references = vec![tcode_core::control::ThreadReference {
            thread_id: "queued".into(),
            by: ReferenceOrigin::User,
            created_at: 1,
            boundary_notice: false,
        }];
        state.sessions.push(source);
        (human, agent)
    });
    let change = |thread_id: &str, op: RunOp| {
        ControlOp::ControlThread(ControlThreadParams {
            changes: vec![RunChange {
                thread_id: thread_id.into(),
                op,
            }],
            request_id: None,
        })
    };
    let (refused, dropped) = state.update(cx, |state, cx| {
        (
            call(
                state,
                cx,
                "caller",
                change("queued", RunOp::DropQueued { id: human }),
            )
            .unwrap(),
            call(
                state,
                cx,
                "caller",
                change("queued", RunOp::DropQueued { id: agent }),
            )
            .unwrap(),
        )
    });
    assert_eq!(refused["results"][0]["outcome"], "failed");
    assert_eq!(
        refused["results"][0]["reason"],
        "refused: dropping a queued human message needs history_edit"
    );
    assert_eq!(dropped["results"][0]["outcome"], "applied");
    let permission = state
        .update(cx, |state, cx| {
            call(
                state,
                cx,
                "caller",
                change(
                    "queued",
                    RunOp::SetOption {
                        id: "permissions".into(),
                        value: serde_json::json!("full_access"),
                    },
                ),
            )
        })
        .unwrap();
    assert_eq!(permission["results"][0]["outcome"], "failed");
    assert_eq!(
        permission["results"][0]["reason"],
        "refused: option permissions sets the permission mode, which only the user changes"
    );
    state.read(|state| {
        assert!(
            state
                .find_meta("queued")
                .unwrap()
                .option_selections
                .iter()
                .all(|selection| selection.id != "permissions"),
            "the permission mode stays the user's"
        );
    });
    state.read(|state| {
        let queue = &state.resident("queued").unwrap().queue;
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].id, human);
    });

    state.update(cx, |state, cx| {
        let mut caller = state.find_meta("caller").unwrap();
        let mut grant = ControlGrant::default_grant();
        grant.categories.insert(ControlCategory::HistoryEdit);
        caller.control_grant = Some(grant);
        state.store_meta(caller, cx);
        let allowed = call(
            state,
            cx,
            "caller",
            change("queued", RunOp::DropQueued { id: human }),
        )
        .unwrap();
        assert_eq!(allowed["results"][0]["outcome"], "applied");
        assert!(state.resident("queued").unwrap().queue.is_empty());
    });

    let forked = state
        .update(cx, |state, cx| {
            call(state, cx, "caller", change("source", RunOp::Fork))
        })
        .unwrap();
    assert_eq!(forked["results"][0]["outcome"], "applied");
    let fork = forked["results"][0]["new_thread_id"].as_str().unwrap();
    assert_ne!(fork, "source");
    state.read(|state| {
        let fork = state.find_meta(fork).unwrap();
        assert_eq!(fork.title, "Thread source (fork)");
        assert_eq!(
            fork.references,
            state.find_meta("source").unwrap().references,
            "a fork inherits the threads its source may read"
        );
    });
}

/// The model references only threads inside its grant's scope; a reference across a project
/// carries the boundary notice and leaves the referenced thread as it was.
#[test]
fn reference_stays_within_the_grant_scope_and_marks_boundaries() {
    let store = TestStore::new("tcode-control-reference");
    let cx = &mut TestAppContext::default();
    let state = host(&store, cx);
    state.update(cx, |state, _| {
        state
            .sessions
            .push(thread("caller", "a", ProviderKind::ClaudeCode));
        state
            .sessions
            .push(thread("near", "a", ProviderKind::ClaudeCode));
        state
            .sessions
            .push(thread("far", "b", ProviderKind::ClaudeCode));
    });
    let reference = |id: &str| {
        ControlOp::Reference(ReferenceParams {
            thread_id: id.into(),
            reason: None,
        })
    };
    let notices = |cx: &mut TestAppContext| -> Vec<RuntimeNotice> {
        cx.drain_outgoing()
            .into_iter()
            .filter_map(|message| match message {
                tcode_protocol::HostMessage::Event(envelope) => match envelope.event {
                    ServerEvent::Runtime(RuntimeEvent::Notice(
                        notice @ RuntimeNotice::ReferenceBoundaryCrossed { .. },
                    )) => Some(notice),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    };
    state.update(cx, |state, cx| {
        assert_eq!(
            call(state, cx, "caller", reference("far")),
            Err("refused: thread far is outside your grant's scope (project)".into())
        );
        let near = call(state, cx, "caller", reference("near")).unwrap();
        assert_eq!(near["boundary_notice"], false);
    });
    cx.run_until_parked();
    assert!(notices(cx).is_empty(), "no boundary, no notice");

    let far_before = state.update(cx, |state, cx| {
        // Observe alone is enough: a reference only makes a thread in scope readable.
        let mut caller = state.find_meta("caller").unwrap();
        caller.control_grant = Some(ControlGrant {
            scope: ControlScope::Host,
            tiers: [ControlTier::Observe].into_iter().collect(),
            ..ControlGrant::none()
        });
        state.store_meta(caller, cx);
        let far_before = state.find_meta("far").unwrap();
        let far = call(state, cx, "caller", reference("far")).unwrap();
        assert_eq!(far["boundary_notice"], true);
        far_before
    });
    cx.run_until_parked();
    assert_eq!(
        notices(cx),
        [RuntimeNotice::ReferenceBoundaryCrossed {
            thread: "caller".into(),
            thread_title: "Thread caller".into(),
            referenced: "far".into(),
            title: "Thread far".into(),
        }],
        "the user hears of the crossing once, when the model makes it"
    );
    state.update(cx, |state, cx| {
        call(state, cx, "caller", reference("far")).unwrap();
    });
    cx.run_until_parked();
    assert!(
        notices(cx).is_empty(),
        "an existing reference is not announced again"
    );

    state.read(|state| {
        assert_eq!(state.find_meta("far").unwrap(), far_before);
        let references = state.find_meta("caller").unwrap().references;
        assert_eq!(
            references
                .iter()
                .map(|reference| (
                    reference.thread_id.as_str(),
                    reference.by,
                    reference.boundary_notice
                ))
                .collect::<Vec<_>>(),
            [
                ("near", ReferenceOrigin::Model, false),
                ("far", ReferenceOrigin::Model, true)
            ]
        );
    });
}

/// `git` answers with what git did: checkout refuses a dirty tree and a thread whose turn is
/// running, commit reports git's output, and a clean tree then checks out.
#[test]
fn git_answers_with_the_outcome_and_refuses_unsafe_checkouts() {
    let store = TestStore::new("tcode-control-git");
    let root = store.root().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    for args in [
        &["init", "-b", "main"][..],
        &["config", "commit.gpgsign", "false"],
        &["config", "core.hooksPath", ".no-hooks"],
        &["config", "user.name", "tcode"],
        &["config", "user.email", "tcode@localhost"],
    ] {
        run_git(&root, args).unwrap();
    }
    std::fs::write(root.join("tracked.txt"), "initial\n").unwrap();
    run_git(&root, &["add", "tracked.txt"]).unwrap();
    run_git(&root, &["commit", "-m", "initial"]).unwrap();
    run_git(&root, &["branch", "topic"]).unwrap();
    std::fs::write(root.join("tracked.txt"), "changed\n").unwrap();

    let cx = &mut TestAppContext::default();
    let state = host(&store, cx);
    state.update(cx, |state, _| {
        state
            .sessions
            .push(thread("caller", "p", ProviderKind::ClaudeCode));
        let mut target = thread("target", "p", ProviderKind::Codex);
        target.cwd = root.clone();
        park(state, ActiveSession::new(target, false, Vec::new()));
    });
    let git = |cx: &mut TestAppContext, op: GitOp| {
        let (reply, response) = smol::channel::bounded(1);
        state.update(cx, |state, cx| {
            state.handle_control_op(
                "caller".into(),
                ControlOp::Git(GitParams {
                    thread_id: "target".into(),
                    op,
                    request_id: None,
                }),
                reply,
                cx,
            )
        });
        cx.run_until(|_| !response.is_empty());
        response.try_recv().unwrap().unwrap()
    };
    let checkout = || GitOp::CheckoutBranch {
        branch: "topic".into(),
    };

    let dirty = git(cx, checkout());
    assert_eq!(dirty["outcome"], "failed");
    assert_eq!(dirty["reason"], "the working tree has uncommitted changes");

    let committed = git(
        cx,
        GitOp::Commit {
            message: "Record the change".into(),
            included: None,
        },
    );
    assert_eq!(committed["outcome"], "applied");
    assert!(
        committed["output"]
            .as_str()
            .unwrap()
            .contains("Record the change")
    );

    state.update(cx, |state, _| {
        state.resident_mut("target").unwrap().turn_in_flight = true;
    });
    let busy = git(cx, checkout());
    assert_eq!(busy["outcome"], "skipped");
    state.update(cx, |state, _| {
        state.resident_mut("target").unwrap().turn_in_flight = false;
    });

    assert_eq!(git(cx, checkout())["outcome"], "applied");
    assert_eq!(
        run_git(&root, &["branch", "--show-current"])
            .unwrap()
            .trim(),
        "topic"
    );
}

/// On the default project-scope grant a quick chat moves itself into a project it just
/// created, which has no threads yet, but not into a project with threads outside its scope;
/// a wider grant reaches that one. Each move is audited in the thread with the project it
/// went to, and the turn that asked keeps its provider until it ends, when the provider
/// leaves the old directory.
#[test]
fn move_thread_moves_the_caller_within_its_grant_and_keeps_its_turn() {
    let store = TestStore::new("tcode-control-move-thread");
    let quick = store.root().join("quick");
    let demo = store.root().join("demo");
    let taken = store.root().join("taken");
    for dir in [&quick, &demo, &taken] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let cx = &mut TestAppContext::default();
    let state = host(&store, cx);
    let (commands, _provider) = smol::channel::unbounded();
    let (demo_id, taken_id) = state.update(cx, |state, cx| {
        let quick_id = state.create_project(quick.clone(), cx).unwrap();
        let demo_id = state.create_project(demo.clone(), cx).unwrap();
        let taken_id = state.create_project(taken.clone(), cx).unwrap();
        let mut owner = thread("owner", &taken_id, ProviderKind::Codex);
        owner.cwd = taken.clone();
        state.persist_meta(&owner, cx);
        let mut caller = thread("caller", &quick_id, ProviderKind::ClaudeCode);
        caller.cwd = quick.clone();
        let mut session = ActiveSession::new(caller, false, Vec::new());
        session.runtime = Runtime::Live(commands);
        session.turn_in_flight = true;
        park(state, session);
        (demo_id, taken_id)
    });
    let move_self = |project_id: &str, request_id: &str| {
        ControlOp::MoveThread(MoveThreadParams {
            thread_id: "caller".into(),
            project_id: project_id.into(),
            cwd: None,
            request_id: Some(request_id.into()),
        })
    };
    let audits = |state: &TestEntity| {
        state.read(|state| {
            state
                .resident("caller")
                .unwrap()
                .timeline
                .entries
                .iter()
                .filter_map(|entry| match &entry.content {
                    EntryContent::ControlAction(action) => {
                        Some((action.action.clone(), action.detail.clone()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
    };

    state.update(cx, |state, cx| {
        assert_eq!(
            call(state, cx, "caller", move_self(&taken_id, "move-1")),
            Err(format!(
                "refused: project {taken_id} is outside your grant's scope (project) and \
                 already has threads"
            ))
        );
        assert_eq!(
            state.find_meta("caller").unwrap().cwd,
            quick,
            "a refused move changes nothing"
        );
        let moved = call(state, cx, "caller", move_self(&demo_id, "move-2")).unwrap();
        assert_eq!(moved["project_id"], demo_id.as_str());
        assert_eq!(moved["cwd"], demo.display().to_string());
    });
    cx.run_until_parked();
    state.read(|state| {
        let session = state.resident("caller").unwrap();
        assert_eq!(session.meta.project_id.as_deref(), Some(demo_id.as_str()));
        assert_eq!(session.meta.cwd, demo);
        assert!(
            matches!(session.runtime, Runtime::Live(_)),
            "the turn that asked keeps its provider"
        );
    });
    assert_eq!(
        audits(&state),
        [(
            tcode_core::control::action::MOVE.to_owned(),
            Some(format!("demo ({})", demo.display()))
        )]
    );
    state.update(cx, |state, cx| {
        state.on_event(
            "caller",
            AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            cx,
        );
        assert!(matches!(
            state.resident("caller").unwrap().runtime,
            Runtime::Idle
        ));
    });

    state.update(cx, |state, cx| {
        let mut caller = state.find_meta("caller").unwrap();
        caller.control_grant = Some(ControlGrant {
            scope: ControlScope::Host,
            tiers: [ControlTier::Operate].into_iter().collect(),
            ..ControlGrant::none()
        });
        state.store_meta(caller, cx);
        call(state, cx, "caller", move_self(&taken_id, "move-3")).unwrap();
        assert_eq!(
            state.find_meta("caller").unwrap().project_id.as_deref(),
            Some(taken_id.as_str())
        );
    });
    cx.run_until_parked();
    assert_eq!(audits(&state).len(), 2);
}
