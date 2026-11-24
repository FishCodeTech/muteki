use super::*;
use crate::app::active_session::Runtime;
use crate::app::test_support::*;
use control_mcp::ReviewItemInput;
use tcode_core::pull_request::{self, PullRequestKey, PullRequestSource};
use tcode_core::pull_request_watch::PullRequestWatch;
use tcode_core::settlement::ThreadActivity;
use tcode_protocol::{ClientPayload, HostMessage, Query, QueryResponse};

fn thread(id: &str, cwd: &Path) -> SessionMeta {
    let mut meta = SessionMeta::new(ProviderKind::ClaudeCode, cwd.to_path_buf(), None);
    meta.id = id.into();
    meta.title = format!("Thread {id}");
    meta.project_id = Some("p".into());
    meta
}

/// Run one `tcode_control` call from `caller` the way the broker does, and wait for its answer.
fn call(
    state: &TestEntity,
    cx: &mut TestAppContext,
    caller: &str,
    op: ControlOp,
) -> Result<serde_json::Value, String> {
    let (reply, answer) = smol::channel::bounded(1);
    state.update(cx, |state, cx| {
        state.handle_control_op(caller.into(), op, reply, cx)
    });
    cx.run_until(|_| !answer.is_empty());
    answer.try_recv().unwrap()
}

fn update(changes: Vec<ThreadChange>, dry_run: bool) -> ControlOp {
    ControlOp::UpdateThreads(UpdateThreadsParams {
        changes,
        dry_run: Some(dry_run),
        request_id: None,
    })
}

fn change(thread_id: &str, op: ThreadOp, expect: Option<Expect>) -> ThreadChange {
    ThreadChange {
        thread_id: thread_id.into(),
        op,
        expect,
    }
}

fn results(answer: &serde_json::Value) -> Vec<BatchResult> {
    serde_json::from_value(answer["results"].clone()).unwrap()
}

/// A preview changes nothing and names what a settle would do; the real batch re-checks every
/// item, skips the running thread as busy and the stale one as changed, and reports the side
/// effects the settle actually had. Unsettle restores none of them.
#[test]
fn update_threads_previews_then_settles_with_its_real_side_effects() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-update-threads");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let (commands, _received) = smol::channel::unbounded();
    let last_activity = now_millis() - 60_000;
    state.update(cx, |state, _| {
        state.sessions.push(thread("caller", store.root()));
        let mut done = thread("done", store.root());
        done.pinned_at = Some(1);
        pull_request::link_pull_request(
            &mut done.pull_requests,
            PullRequestKey::new("github.com", "sample/project", 7),
            "https://github.com/sample/project/pull/7".into(),
            PullRequestSource::Manual,
            1,
            true,
        );
        done.pull_requests[0].watch = Some(PullRequestWatch::new(1));
        let mut session = ActiveSession::new(done.clone(), false, Vec::new());
        session.runtime = Runtime::Live(commands);
        session.push_or_merge_orchestrate_callback("[orchestrate] a child finished".into());
        state.sessions.push(done);
        state.residents.parked.insert("done".into(), session);

        let running = thread("running", store.root());
        let mut session = ActiveSession::new(running.clone(), false, Vec::new());
        session.turn_in_flight = true;
        state.sessions.push(running);
        state.residents.parked.insert("running".into(), session);

        state.sessions.push(thread("stale", store.root()));
        let mut far = thread("far", store.root());
        far.project_id = Some("elsewhere".into());
        state.sessions.push(far);
        for id in ["done", "running", "stale"] {
            state.thread_activity.insert(
                id.into(),
                ThreadActivity {
                    last_run_completed_at: Some(last_activity),
                    ..Default::default()
                },
            );
        }
    });
    let batch = |dry_run| {
        update(
            vec![
                change(
                    "done",
                    ThreadOp::Settle,
                    Some(Expect {
                        last_activity_at: Some(last_activity),
                        state: Some("active".into()),
                        ..Default::default()
                    }),
                ),
                change("running", ThreadOp::Settle, None),
                change(
                    "stale",
                    ThreadOp::Settle,
                    Some(Expect {
                        max_position: Some(41),
                        ..Default::default()
                    }),
                ),
                change("far", ThreadOp::Settle, None),
            ],
            dry_run,
        )
    };
    let expected_effects = vec![
        "provider detached".to_owned(),
        "1 PR watch ended".to_owned(),
        "1 non-Human queued message dropped".to_owned(),
        "unpinned".to_owned(),
    ];
    let skipped = |target: &str, reason: &str| BatchResult {
        target: target.into(),
        outcome: Outcome::Skipped,
        reason: Some(reason.into()),
        side_effects: None,
    };

    let preview = call(&state, cx, "caller", batch(true)).unwrap();
    assert_eq!(
        results(&preview),
        vec![
            BatchResult {
                target: "done".into(),
                outcome: Outcome::WouldApply,
                reason: None,
                side_effects: Some(expected_effects.clone()),
            },
            skipped("running", "busy"),
            skipped("stale", "changed_since_preview"),
            skipped("far", "outside_scope: your grant's scope is project"),
        ]
    );
    state.read(|state| {
        let done = state.find_meta("done").unwrap();
        assert!(!done.is_settled() && done.pinned_at.is_some());
        assert!(done.pull_requests[0].watch.is_some());
        assert_eq!(state.resident("done").unwrap().queue.len(), 1);
        assert!(
            state.control_activity(10).is_empty(),
            "a preview audits nothing"
        );
    });

    let applied = call(&state, cx, "caller", batch(false)).unwrap();
    assert_eq!(
        results(&applied),
        vec![
            BatchResult {
                target: "done".into(),
                outcome: Outcome::Applied,
                reason: None,
                side_effects: Some(expected_effects),
            },
            skipped("running", "busy"),
            skipped("stale", "changed_since_preview"),
            skipped("far", "outside_scope: your grant's scope is project"),
        ]
    );
    state.read(|state| {
        let done = state.find_meta("done").unwrap();
        assert!(done.is_settled() && done.pinned_at.is_none());
        assert!(done.pull_requests[0].watch.is_none());
        let session = state.resident("done").unwrap();
        assert!(session.queue.is_empty());
        assert!(matches!(session.runtime, Runtime::Idle));
        assert!(session.timeline.entries.iter().any(|entry| matches!(
            &entry.content,
            EntryContent::ControlAction(action)
                if action.actor_thread_id == "caller" && action.action == "settle"
        )));
        assert!(!state.find_meta("running").unwrap().is_settled());
        assert!(!state.find_meta("stale").unwrap().is_settled());
        assert!(!state.find_meta("far").unwrap().is_settled());
    });

    let stale_state = call(
        &state,
        cx,
        "caller",
        update(
            vec![change(
                "done",
                ThreadOp::Unsettle,
                Some(Expect {
                    state: Some("active".into()),
                    ..Default::default()
                }),
            )],
            false,
        ),
    )
    .unwrap();
    assert_eq!(
        results(&stale_state),
        vec![skipped("done", "changed_since_preview")]
    );
    let unsettled = call(
        &state,
        cx,
        "caller",
        update(vec![change("done", ThreadOp::Unsettle, None)], false),
    )
    .unwrap();
    assert_eq!(
        results(&unsettled),
        vec![BatchResult {
            target: "done".into(),
            outcome: Outcome::Applied,
            reason: None,
            side_effects: Some(vec!["override becomes active; nothing is restored".into()]),
        }]
    );
    state.read(|state| {
        let done = state.find_meta("done").unwrap();
        assert_eq!(done.settled_override, Some(SettledOverride::Active));
        assert!(done.pinned_at.is_none() && done.pull_requests[0].watch.is_none());
    });
}

/// Follow-up, parked and the Suggest tier hold back the automatic rules a plain thread falls
/// to; releasing each lets the rule apply.
#[test]
fn follow_up_parked_and_the_suggest_tier_hold_automatic_settlement() {
    let store = TestStore::new("tcode-control-protections");
    let stamp = now_millis() - 10 * 86_400_000;
    for id in ["plain", "follow", "parked"] {
        let mut meta = thread(id, store.root());
        match id {
            "follow" => {
                meta.follow_up = Some(FollowUp {
                    since: 1,
                    note: None,
                })
            }
            "parked" => meta.parked_at = Some(1),
            _ => {}
        }
        store.upsert_meta(&meta).unwrap();
        store
            .append_event(
                id,
                stamp,
                &AgentEvent::TurnCompleted {
                    turn_id: "finished".into(),
                    status: agent::TurnStatus::Completed,
                    usage: None,
                },
            )
            .unwrap();
    }
    std::fs::write(
        store.root().join("settings.json"),
        r#"{"settlement_automation":"suggest"}"#,
    )
    .unwrap();
    let cx = &mut TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, cx| state.start_activity(cx));
    cx.run_until_parked();
    let settled = |state: &TestEntity| {
        state.read(|state| {
            ["plain", "follow", "parked"].map(|id| state.find_meta(id).unwrap().is_settled())
        })
    };
    assert_eq!(
        settled(&state),
        [false, false, false],
        "suggest settles nothing"
    );

    state.dispatch_command(
        cx,
        1,
        Command::PatchSettings {
            patch: SettingsPatch::SettlementAutomation(
                tcode_core::control::SettlementAutomation::ExplicitSignals,
            ),
        },
    );
    cx.run_until_parked();
    assert_eq!(settled(&state), [true, false, false]);

    state.dispatch_command(
        cx,
        2,
        Command::SetFollowUp {
            session_id: "follow".into(),
            follow_up: None,
        },
    );
    state.dispatch_command(
        cx,
        3,
        Command::SetParked {
            session_id: "parked".into(),
            parked: false,
        },
    );
    cx.run_until_parked();
    assert_eq!(settled(&state), [true, true, true]);
}

/// A review is stored and announced without touching any thread, and survives a restart.
#[test]
fn a_recorded_review_changes_no_thread_and_is_read_back_after_a_restart() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-review");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, _| {
        state.sessions.push(thread("caller", store.root()));
        state.sessions.push(thread("old", store.root()));
    });
    let before = state.read(|state| state.find_meta("old").unwrap());
    let review = |group: &str| {
        ControlOp::RecordReview(RecordReviewParams {
            items: vec![ReviewItemInput {
                thread_id: "old".into(),
                group: group.into(),
                confidence: "clear_evidence".into(),
                evidence: vec!["old#12: PR merged".into()],
                suggestion: Some("settle".into()),
            }],
            coverage: "1 project, 2 threads".into(),
            request_id: Some("review-1".into()),
        })
    };
    assert!(call(&state, cx, "caller", review("done")).is_err());
    let recorded = call(&state, cx, "caller", review("can_settle")).unwrap();
    assert_eq!(recorded["status"], "recorded");
    assert_eq!(
        call(&state, cx, "caller", review("can_settle")).unwrap(),
        recorded,
        "a retry records nothing twice"
    );
    assert!(cx.drain_outgoing().iter().any(|message| matches!(
        message,
        HostMessage::Event(envelope) if matches!(
            &envelope.event,
            ServerEvent::Runtime(RuntimeEvent::Notice(RuntimeNotice::ReviewRecorded { items: 1, .. }))
        )
    )));
    assert_eq!(state.read(|state| state.find_meta("old").unwrap()), before);
    state.update(cx, |state, cx| {
        let flush = state.store_write_barrier(cx);
        HostCx::spawn_detached(cx, async move {
            let _ = flush.recv().await;
        });
    });
    cx.run_until_parked();

    let restarted = cx.new_entity(TestClientState::new((*store).clone()));
    restarted.deliver(
        cx,
        5,
        ClientPayload::Query(Query::ReviewRecords { limit: 10 }),
    );
    let (records, _) = cx.run_until_sent(|message| match message {
        HostMessage::QueryResult {
            id: 5,
            result: Ok(QueryResponse::ReviewRecords(records)),
        } => Some(records.clone()),
        _ => None,
    });
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, "review-1");
    assert_eq!(records[0].thread_id, "caller");
    assert_eq!(records[0].items[0].group, "can_settle");
}
