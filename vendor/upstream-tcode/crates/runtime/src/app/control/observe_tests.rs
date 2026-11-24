use super::*;
use crate::app::test_support::*;
use tcode_core::control::{ControlScope, FollowUp, ReferenceOrigin, ThreadReference};
use tcode_core::settlement::ThreadActivity;

const HOUR_MS: u64 = 3_600_000;

fn thread(id: &str, project: &str) -> SessionMeta {
    let mut meta = SessionMeta::new(
        ProviderKind::ClaudeCode,
        PathBuf::from("/tmp/synthetic-checkout"),
        None,
    );
    meta.id = id.into();
    meta.title = format!("Thread {id}");
    meta.project_id = Some(project.into());
    meta
}

fn active_at(at: u64) -> ThreadActivity {
    ThreadActivity {
        last_message_at: Some(at),
        last_human_message_at: Some(at),
        last_run_started_at: Some(at),
        last_run_completed_at: Some(at),
        failed: false,
        interrupted: false,
    }
}

fn add(state: &mut AppState, meta: SessionMeta, activity_at: Option<u64>) {
    if let Some(at) = activity_at {
        state.thread_activity.insert(meta.id.clone(), active_at(at));
    }
    state.sessions.push(meta);
}

/// One `tcode_control` call from the thread `caller`, answered the way its MCP server is.
fn call(
    state: &TestEntity,
    cx: &mut TestAppContext,
    op: ControlOp,
) -> Result<serde_json::Value, String> {
    let (reply, response) = smol::channel::bounded(1);
    state.update(cx, |state, cx| {
        state.handle_control_op("caller".into(), op, reply, cx)
    });
    smol::block_on(response.recv()).unwrap()
}

fn ids(page: &serde_json::Value, key: &str) -> Vec<String> {
    page[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap().to_owned())
        .collect()
}

/// `list_threads` lists the caller's project by default, hides orchestrated children and
/// archived threads unless asked, orders by last activity, and pages without gaps or repeats.
#[test]
fn list_threads_honours_scope_hides_children_and_archived_and_pages() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-list-threads");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let now = now_millis();
    state.update(cx, |state, _| {
        add(state, thread("caller", "a"), Some(now - 5 * HOUR_MS));
        add(state, thread("newest", "a"), Some(now - HOUR_MS));
        add(state, thread("middle", "a"), Some(now - 2 * HOUR_MS));
        let mut settled = thread("settled", "a");
        settled.settled_at = Some(now / 1000);
        add(state, settled, Some(now - 3 * HOUR_MS));
        let mut child = thread("child", "a");
        child.parent_session_id = Some("caller".into());
        add(state, child, Some(now - HOUR_MS / 2));
        let mut archived = thread("archived", "a");
        archived.archived_at = Some(now / 1000);
        add(state, archived, Some(now - 4 * HOUR_MS));
        add(state, thread("elsewhere", "b"), Some(now));
    });

    let page = call(
        &state,
        cx,
        ControlOp::ListThreads(ListThreadsParams::default()),
    )
    .unwrap();
    assert_eq!(
        ids(&page, "threads"),
        ["newest", "middle", "settled", "caller"]
    );
    assert_eq!(page["threads"][2]["state"], "settled");

    let page = call(
        &state,
        cx,
        ControlOp::ListThreads(ListThreadsParams {
            include_children: Some(true),
            states: Some(vec![ThreadState::Active, ThreadState::Archived]),
            ..ListThreadsParams::default()
        }),
    )
    .unwrap();
    assert_eq!(
        ids(&page, "threads"),
        ["child", "newest", "middle", "archived", "caller"]
    );

    let mut paged = Vec::new();
    let mut cursor = None;
    loop {
        let page = call(
            &state,
            cx,
            ControlOp::ListThreads(ListThreadsParams {
                limit: Some(1),
                cursor: cursor.clone(),
                ..ListThreadsParams::default()
            }),
        )
        .unwrap();
        paged.extend(ids(&page, "threads"));
        match page["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
    }
    assert_eq!(paged, ["newest", "middle", "settled", "caller"]);

    assert_eq!(
        call(
            &state,
            cx,
            ControlOp::ListThreads(ListThreadsParams {
                scope: Some(ControlScope::Host),
                ..ListThreadsParams::default()
            })
        ),
        Err("refused: scope host is wider than your grant's project".into())
    );
    state.update(cx, |state, _| {
        let mut caller = state.find_meta("caller").unwrap();
        caller.control_grant = Some(ControlGrant {
            scope: ControlScope::Host,
            ..ControlGrant::default_grant()
        });
        state.sessions.retain(|meta| meta.id != "caller");
        state.sessions.push(caller);
    });
    let page = call(
        &state,
        cx,
        ControlOp::ListThreads(ListThreadsParams::default()),
    )
    .unwrap();
    assert_eq!(ids(&page, "threads")[0], "elsewhere");
}

/// For an unsettled thread, `get_thread` names why automatic settlement does not apply, and
/// gives the due time when it would.
#[test]
fn get_thread_explains_why_an_unsettled_thread_was_not_auto_settled() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-settlement-explanation");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let idle_since = now_millis() - 24 * HOUR_MS;
    state.update(cx, |state, _| {
        add(state, thread("caller", "a"), Some(idle_since));
        add(state, thread("idle", "a"), Some(idle_since));
        let mut unsettled = thread("unsettled", "a");
        unsettled.settled_override = Some(SettledOverride::Active);
        add(state, unsettled, Some(idle_since));
        let mut pinned = thread("pinned", "a");
        pinned.pinned_at = Some(idle_since / 1000);
        add(state, pinned, Some(idle_since));
        let mut follow_up = thread("follow_up", "a");
        follow_up.follow_up = Some(FollowUp {
            since: idle_since / 1000,
            note: None,
        });
        add(state, follow_up, Some(idle_since));
        let queued = thread("queued", "a");
        add(state, queued.clone(), Some(idle_since));
        let mut session = ActiveSession::new(queued, false, Vec::new());
        session.push_or_merge_orchestrate_callback("[orchestrate] child finished".into());
        state.residents.parked.insert("queued".into(), session);
    });
    let explain = |state: &TestEntity, cx: &mut TestAppContext, id: &str| {
        call(
            state,
            cx,
            ControlOp::GetThread(GetThreadParams {
                thread_id: id.into(),
            }),
        )
        .unwrap()["settlement"]
            .clone()
    };

    let idle = explain(&state, cx, "idle");
    assert_eq!(idle["would_auto_settle"], true);
    assert_eq!(idle["reasons"], serde_json::json!([]));
    assert_eq!(idle["due_at"], idle_since + 3 * 24 * HOUR_MS + 1);
    for (id, reason) in [
        ("unsettled", "override_active"),
        ("pinned", "pinned"),
        ("follow_up", "follow_up"),
        ("queued", "pending_agent_message"),
    ] {
        let explanation = explain(&state, cx, id);
        assert_eq!(explanation["would_auto_settle"], false, "{id}");
        assert_eq!(explanation["due_at"], serde_json::Value::Null, "{id}");
        assert_eq!(explanation["reasons"], serde_json::json!([reason]), "{id}");
    }
}

/// `search` reads only threads the caller may read: its scope, itself and its references.
#[test]
fn search_is_limited_to_readable_threads_and_returns_anchors() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-search");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    for id in ["caller", "same", "other", "referenced"] {
        store
            .append_event(
                id,
                1,
                &AgentEvent::TurnStarted {
                    turn_id: "t".into(),
                },
            )
            .unwrap();
        let reply = AgentEvent::ItemCompleted(ThreadItem {
            id: "reply".into(),
            parent_item_id: None,
            content: ItemContent::AssistantMessage {
                text: format!("{id} fixed the needle bug"),
            },
        });
        store.append_event(id, 2, &reply).unwrap();
    }
    state.update(cx, |state, _| {
        add(state, thread("same", "a"), None);
        add(state, thread("other", "b"), None);
        add(state, thread("referenced", "b"), None);
        let mut caller = thread("caller", "a");
        caller.references.push(ThreadReference {
            thread_id: "referenced".into(),
            by: ReferenceOrigin::User,
            created_at: 0,
            boundary_notice: true,
        });
        add(state, caller, None);
    });
    let search = |thread_ids: Option<Vec<String>>| {
        ControlOp::Search(SearchParams {
            query: "needle".into(),
            thread_ids,
            ..SearchParams::default()
        })
    };

    let answer = call(&state, cx, search(None)).unwrap();
    let mut anchors: Vec<&str> = answer["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["anchor"].as_str().unwrap())
        .collect();
    anchors.sort();
    assert_eq!(anchors, ["caller#1", "referenced#1", "same#1"]);
    assert_eq!(answer["total"], 3);
    assert!(
        answer["hits"][0]["snippet"]
            .as_str()
            .unwrap()
            .contains("begin quoted data"),
        "snippets are fenced as data"
    );

    let answer = call(&state, cx, search(Some(vec!["referenced".into()]))).unwrap();
    assert_eq!(answer["hits"][0]["anchor"], "referenced#1");
    assert_eq!(answer["total"], 1);
    assert_eq!(
        call(&state, cx, search(Some(vec!["other".into()]))),
        Err("refused: thread other is outside your grant's scope (project)".into())
    );
}

/// `review_candidates` changes nothing, finds old unresolved work, and states its coverage.
#[test]
fn review_candidates_changes_no_state_and_reports_coverage() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-review");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let now = now_millis();
    let old = now - 40 * 24 * HOUR_MS;
    let reply = AgentEvent::ItemCompleted(ThreadItem {
        id: "reply".into(),
        parent_item_id: None,
        content: ItemContent::AssistantMessage {
            text: "Next step is wiring the importer.".into(),
        },
    });
    let completed = AgentEvent::TurnCompleted {
        turn_id: "t".into(),
        status: TurnStatus::Completed,
        usage: None,
    };
    for (ts, event) in [
        (
            old - 1000,
            AgentEvent::TurnStarted {
                turn_id: "t".into(),
            },
        ),
        (old - 500, reply),
        (old, completed),
    ] {
        store.append_event("forgotten", ts, &event).unwrap();
    }
    state.update(cx, |state, _| {
        state.projects.push(Project {
            id: "a".into(),
            name: "a".into(),
            root: PathBuf::from("/tmp/synthetic-checkout"),
            quick_chat: false,
            icon_path: None,
            permission_defaults: Default::default(),
            created_at: 0,
        });
        add(state, thread("caller", "a"), Some(now));
        add(state, thread("forgotten", "a"), Some(old));
        let mut unsettled = thread("unsettled", "a");
        unsettled.settled_override = Some(SettledOverride::Active);
        add(state, unsettled, Some(now - 2 * 24 * HOUR_MS));
        let mut settled = thread("long_settled", "a");
        settled.settled_at = Some(old / 1000);
        add(state, settled, Some(old));
        let mut child = thread("child", "a");
        child.parent_session_id = Some("forgotten".into());
        add(state, child, Some(old));
    });
    let snapshot = |state: &TestEntity| {
        state.read(|state| {
            (
                state.sessions.iter().cloned().collect::<Vec<_>>(),
                (*state.thread_activity).clone(),
                state.settings.last_visited.clone(),
            )
        })
    };
    let before = snapshot(&state);

    let review = call(
        &state,
        cx,
        ControlOp::ReviewCandidates(ReviewCandidatesParams::default()),
    )
    .unwrap();
    cx.run_until_parked();
    assert!(snapshot(&state) == before, "a review changes no state");

    assert_eq!(ids(&review, "candidates"), ["unsettled", "forgotten"]);
    let unsettled = &review["candidates"][0];
    assert_eq!(unsettled["manual_unsettle"], true);
    assert_eq!(
        unsettled["settlement"]["reasons"],
        serde_json::json!(["override_active"])
    );
    let forgotten = &review["candidates"][1];
    assert_eq!(forgotten["idle_days"], 40);
    assert!(
        forgotten["last_turn"]["assistant_head"]
            .as_str()
            .unwrap()
            .contains("Next step is wiring the importer."),
        "{forgotten}"
    );
    let collapsed: Vec<(&str, &str)> = review["collapsed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| (row["id"].as_str().unwrap(), row["reason"].as_str().unwrap()))
        .collect();
    assert_eq!(
        collapsed,
        [("caller", "just_active"), ("child", "orchestrate_child")]
    );
    assert_eq!(review["coverage"]["projects"], 1);
    assert_eq!(review["coverage"]["threads"], 5);
    assert_eq!(review["coverage"]["without_enough_data"], 1);
}

/// `get_settings` shows the settings without environment values, keys this build does not
/// know, or token-shaped strings, and says how many it redacted.
#[test]
fn get_settings_leaves_out_env_values_unknown_keys_and_tokens() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-get-settings");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, _| {
        add(state, thread("caller", "p"), None);
        state.settings.profiles.insert(
            "team".into(),
            tcode_core::settings::ProviderProfile {
                kind: ProviderKind::Codex,
                settings: tcode_core::settings::ProviderSettings {
                    env: vec![tcode_core::settings::EnvVar {
                        name: "BASE_URL".into(),
                        value: "https://private-endpoint.invalid".into(),
                        sensitive: false,
                    }],
                    ..tcode_core::settings::ProviderSettings::default()
                },
            },
        );
        state.settings.browser.home_url =
            Some("https://example.invalid/?t=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij".into());
        state.settings.unknown.insert(
            "future_secret".into(),
            serde_json::json!("from a newer build"),
        );
    });
    let answer = call(
        &state,
        cx,
        ControlOp::GetSettings(control_mcp::GetSettingsParams::default()),
    )
    .unwrap();
    let text = answer.to_string();
    assert!(!text.contains("private-endpoint"), "{text}");
    assert!(!text.contains("future_secret"), "{text}");
    assert!(!text.contains("ghp_ABCDEF"), "{text}");
    assert_eq!(
        answer["settings"]["profiles"]["team"]["env"][0]["name"],
        "BASE_URL"
    );
    assert_eq!(answer["redacted"], 1);
}

/// However many threads a review collapses, it names a bounded number and says how many
/// more there are.
#[test]
fn review_candidates_bounds_the_collapsed_list() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-review-cap");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, _| {
        add(state, thread("caller", "p"), None);
        for index in 0..150 {
            let mut child = thread(&format!("child-{index}"), "p");
            child.parent_session_id = Some("caller".into());
            add(state, child, None);
        }
    });
    let review = call(
        &state,
        cx,
        ControlOp::ReviewCandidates(control_mcp::ReviewCandidatesParams::default()),
    )
    .unwrap();
    assert_eq!(review["collapsed"].as_array().unwrap().len(), 100);
    // The 150 children and the caller itself, which was just active.
    assert!(
        review["omitted"].as_array().unwrap().iter().any(|note| note
            .as_str()
            .unwrap()
            .starts_with("51 more collapsed threads")),
        "{}",
        review["omitted"]
    );
}
