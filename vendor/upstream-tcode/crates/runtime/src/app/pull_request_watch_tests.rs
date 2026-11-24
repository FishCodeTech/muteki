use super::*;
use crate::app::test_support::*;
use serde_json::{Value, json};
use std::sync::{Mutex, mpsc};
use tcode_core::pull_request::PullRequestSnapshot;
use tcode_protocol::{Command, EventEnvelope, HostMessage, RuntimeNotification, ServerEvent};
use tcode_services::{
    github::{Credentials, GitHub, GitHubApi},
    settings::SettingsStore,
};

use crate::app::test_support::github_fixture as fixture;

const START: &str = "2026-01-01T00:00:00.000Z";
const DETAIL: &str = "PullRequestWatchDetail";
const ACTIVITY: &str = "PullRequestWatchActivity";
const THREADS: &str = "PullRequestWatchThreads";
const TAIL: &str = "PullRequestWatchThreadComments";
const FINGERPRINTS: &str = "PullRequestWatchFingerprints";
const BRANCH: &str = "GET /repos/sample/project/branches/main";

struct ReviewThread {
    id: &'static str,
    comments: Vec<Value>,
}

/// GitHub as the watch reads it, for pull request #1 of sample/project.
struct Host {
    state: &'static str,
    head: &'static str,
    checks: Vec<Value>,
    /// The checks main's branch protection requires.
    protected: Vec<&'static str>,
    /// The checks a ruleset on main requires.
    ruled: Vec<&'static str>,
    mergeable: &'static str,
    comments: Vec<Value>,
    /// The comment list always has another page.
    endless_comments: bool,
    threads: Vec<ReviewThread>,
    fingerprints: bool,
    rate_limited: Option<&'static str>,
    failing: Option<&'static str>,
    /// Holds the next detail read until the test answers it.
    gate: Option<mpsc::Receiver<()>>,
    requests: Vec<String>,
}

fn check(name: &str, conclusion: &str) -> Value {
    json!({
        "__typename": "CheckRun",
        "name": name,
        "status": if conclusion == "PENDING" { "IN_PROGRESS" } else { "COMPLETED" },
        "conclusion": if conclusion == "PENDING" { Value::Null } else { json!(conclusion) },
        "detailsUrl": format!("https://ci.test/{name}"),
    })
}
fn required(name: &str, conclusion: &str) -> Value {
    let mut check = check(name, conclusion);
    check["isRequired"] = json!(true);
    check
}
fn comment(id: &str, author: &str, at: &str) -> Value {
    json!({
        "id": id,
        "body": format!("note {id}"),
        "createdAt": at,
        "lastEditedAt": null,
        "url": format!("https://github.com/sample/project/pull/1#{id}"),
        "author": {"login": author},
    })
}
fn reply(host: &Mutex<Host>, exchange: fixture::Exchange) {
    if exchange.request.starts_with(BRANCH) {
        let mut host = host.lock().unwrap();
        host.requests.push(BRANCH.into());
        let body = json!({"name": "main", "protection": {
            "enabled": true,
            "required_status_checks": {"contexts": host.protected},
        }});
        return exchange.reply(200, "", &serde_json::to_vec(&body).unwrap());
    }
    let sent: Value = serde_json::from_slice(&exchange.body).unwrap();
    let operation = sent["query"]
        .as_str()
        .unwrap()
        .split([' ', '('])
        .nth(1)
        .unwrap()
        .to_owned();
    let gate = {
        let mut host = host.lock().unwrap();
        host.requests.push(operation.clone());
        if operation == DETAIL {
            host.gate.take()
        } else {
            None
        }
    };
    if let Some(gate) = gate {
        let _ = gate.recv();
    }
    let host = host.lock().unwrap();
    if host
        .rate_limited
        .is_some_and(|limited| limited == operation || limited == "*")
    {
        return exchange.reply(
            403,
            "x-ratelimit-remaining: 0\r\nx-ratelimit-reset: 4102444800\r\n",
            br#"{"message":"API rate limit exceeded"}"#,
        );
    }
    if host.failing == Some(operation.as_str()) {
        return exchange.reply(502, "", b"{}");
    }
    let page = |nodes: &[Value], more: bool| {
        json!({
            "pageInfo": {"hasNextPage": more, "endCursor": more.then(|| format!("c{}", host.requests.len()))},
            "nodes": nodes,
        })
    };
    let data = match operation.as_str() {
        FINGERPRINTS => {
            let mut counts = HashMap::<String, u64>::new();
            for check in &host.checks {
                let state = check["conclusion"]
                    .as_str()
                    .or_else(|| check["status"].as_str())
                    .unwrap();
                *counts.entry(state.into()).or_default() += 1;
            }
            let edited = |rows: &[Value]| {
                rows.iter()
                    .map(|row| json!({"lastEditedAt": row["lastEditedAt"]}))
                    .collect::<Vec<_>>()
            };
            let mut data = serde_json::Map::new();
            for name in sent["variables"].as_object().unwrap().keys() {
                if let Some(alias) = name.strip_suffix("_number") {
                    let pull_request = host.fingerprints.then(|| {
                        json!({
                            "state": host.state,
                            "mergeable": host.mergeable,
                            "headRefOid": host.head,
                            "comments": {"totalCount": host.comments.len(), "nodes": edited(&host.comments)},
                            "reviews": {"totalCount": 0, "nodes": []},
                            "reviewThreads": {"totalCount": host.threads.len()},
                            "commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {
                                "checkRunCountsByState": counts.iter().map(|(state, count)| json!({"state": state, "count": count})).collect::<Vec<_>>(),
                                "statusContextCountsByState": [],
                            }}}}]},
                        })
                    });
                    data.insert(alias.into(), json!({ "pullRequest": pull_request }));
                }
            }
            Value::Object(data)
        }
        DETAIL => json!({
            "viewer": {"login": "tcode-bot"},
            "repository": {"pullRequest": {
                "number": 1,
                "state": host.state,
                "mergedAt": (host.state == "MERGED").then_some("2026-01-02T00:00:00Z"),
                "mergeable": host.mergeable,
                "headRefOid": host.head,
                "baseRefName": "main",
                "baseRef": {"rules": {"nodes": [{"parameters": {"requiredStatusChecks": host
                    .ruled
                    .iter()
                    .map(|context| json!({"context": context}))
                    .collect::<Vec<_>>()}}]}},
                "author": {"login": "tcode-bot"},
                "commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": page(&host.checks, false)}}}]},
            }},
        }),
        ACTIVITY => json!({"repository": {"pullRequest": {
            "comments": page(&host.comments, host.endless_comments),
            "reviews": page(&[], false),
        }}}),
        THREADS => json!({"repository": {"pullRequest": {"reviewThreads": page(
            &host
                .threads
                .iter()
                .map(|thread| json!({
                    "id": thread.id,
                    "path": "src/lib.rs",
                    "comments": {
                        "totalCount": thread.comments.len(),
                        "pageInfo": {"hasNextPage": thread.comments.len() > 10, "endCursor": "first"},
                        "nodes": thread.comments[..thread.comments.len().min(10)],
                    },
                }))
                .collect::<Vec<_>>(),
            false,
        )}}}),
        TAIL => {
            let thread = host
                .threads
                .iter()
                .find(|thread| sent["variables"]["thread"] == thread.id)
                .unwrap();
            json!({"node": {"comments": page(&thread.comments[10..], false)}})
        }
        _ => json!({}),
    };
    exchange.reply(
        200,
        "",
        &serde_json::to_vec(&json!({ "data": data })).unwrap(),
    );
}

struct Harness {
    dir: TestStore,
    store: SessionStore,
    host: Arc<Mutex<Host>>,
    cx: TestAppContext,
    state: TestEntity,
    commands: smol::channel::Receiver<SessionCommand>,
    events: smol::channel::Sender<AgentEvent>,
    _server: fixture::Server,
}

impl Harness {
    fn new(prefix: &str) -> Self {
        let dir = TestStore::new(prefix);
        let store = (*dir).clone();
        Self::open(dir, store, Arc::new(Mutex::new(Host::default())))
    }

    /// Close this host and open a fresh one over the same data, as a restart does.
    fn restart(mut self) -> Self {
        self.state
            .update(&mut self.cx, |state, _| state.close_store())
            .unwrap();
        let Self { dir, host, .. } = self;
        let store = SessionStore::open_at(dir.root().clone()).unwrap();
        let mut restarted = Self::open(dir, store, host);
        let sessions = restarted.store.load_index().unwrap();
        restarted.state.update(&mut restarted.cx, |state, _| {
            state.sessions = sessions.into()
        });
        restarted
    }

    fn open(dir: TestStore, store: SessionStore, host: Arc<Mutex<Host>>) -> Self {
        let fixture = fixture::Fixture::new();
        let api = GitHub::new(GitHubApi::new(
            Credentials::new(
                SettingsStore::new(store.root().to_path_buf()),
                [("GH_TOKEN".into(), "fixture".into())],
            ),
            fixture.builder(),
        ));
        let serving = host.clone();
        let server = fixture.serve(move |exchange| reply(&serving, exchange));
        let scripted = scripted_provider(ProviderKind::Codex);
        let mut cx = TestAppContext::default();
        let state = cx.new_entity({
            let mut state = TestClientState::new(store.clone());
            state.set_provider_launcher_for_test(scripted.launcher);
            state.pull_request_watches = WatchRuntime::new(api);
            state
        });
        Self {
            dir,
            store,
            host,
            cx,
            state,
            commands: scripted.commands,
            events: scripted.events,
            _server: server,
        }
    }

    fn host(&self) -> std::sync::MutexGuard<'_, Host> {
        self.host.lock().unwrap()
    }

    fn watch_thread(&mut self, id: &str, started_at: &str) {
        let mut meta = SessionMeta::new(
            ProviderKind::Codex,
            PathBuf::from("/tmp/synthetic-checkout"),
            None,
        );
        meta.id = id.into();
        meta.project_id = Some("sample".into());
        pull_request::link_pull_request(
            &mut meta.pull_requests,
            key(),
            "https://github.com/sample/project/pull/1".into(),
            PullRequestSource::Manual,
            1,
            true,
        );
        let mut watch = PullRequestWatch::new(0);
        watch.started_at = started_at.into();
        watch.remarks_through = started_at.into();
        meta.pull_requests[0].watch = Some(watch);
        self.store.upsert_meta(&meta).unwrap();
        self.state
            .update(&mut self.cx, |state, _| state.sessions.push(meta));
    }

    fn pass(&mut self) {
        self.state.update(&mut self.cx, |state, cx| {
            state.sweep_pull_request_watches(cx).detach()
        });
        self.cx
            .run_until(|state| !state.pull_request_watches.passing);
    }

    fn watch(&self, id: &str) -> Option<PullRequestWatch> {
        self.state
            .read(|state| state.find_meta(id).unwrap().pull_requests[0].watch.clone())
    }

    /// Every request since the last call.
    fn requests(&self) -> Vec<String> {
        std::mem::take(&mut self.host().requests)
    }

    /// The text of the next turn the scripted provider receives.
    fn delivered(&mut self) -> (u64, String) {
        let commands = self.commands.clone();
        self.cx.run_until(|_| !commands.is_empty());
        match commands.try_recv() {
            Ok(SessionCommand::SendTurn {
                delivery_id, text, ..
            }) => (delivery_id, text),
            other => panic!("expected a delivered turn, got {other:?}"),
        }
    }

    /// The provider takes the turn and finishes it.
    fn accept(&mut self, delivery_id: u64) {
        self.events
            .try_send(AgentEvent::TurnAccepted { delivery_id })
            .unwrap();
        self.finish_turn();
    }

    /// The provider finishes the running turn.
    fn finish_turn(&mut self) {
        self.events
            .try_send(AgentEvent::TurnCompleted {
                turn_id: "turn".into(),
                status: TurnStatus::Completed,
                usage: None,
            })
            .unwrap();
        // The host's event pump queues each event for the host as it takes it,
        // whether or not the host still acts on it.
        let events = self.events.clone();
        self.cx.run_until(|_| events.is_empty());
        self.cx.run_until_parked();
    }

    /// The text of the next wake steered into the running turn.
    fn steered(&mut self) -> String {
        let commands = self.commands.clone();
        self.cx.run_until(|_| !commands.is_empty());
        match commands.try_recv() {
            Ok(SessionCommand::Steer { text, .. }) => text,
            other => panic!("expected a steered wake, got {other:?}"),
        }
    }

    /// The user presses Stop on the thread; the host's answer.
    fn stop(&mut self, id: u64) -> Result<(), ProtocolError> {
        self.state.dispatch_command(
            &mut self.cx,
            id,
            Command::Interrupt {
                session_id: "thread".into(),
            },
        );
        self.cx
            .drain_outgoing()
            .into_iter()
            .find_map(|message| match message {
                HostMessage::Ack { id: acked, result } if acked == id => Some(result.map(drop)),
                _ => None,
            })
            .expect("Stop is acknowledged")
    }

    fn pause(&mut self, id: u64) {
        self.state.dispatch_command(
            &mut self.cx,
            id,
            Command::SetPullRequestWatchesPaused {
                session_id: "thread".into(),
                paused: true,
            },
        );
        let acked = self
            .cx
            .drain_outgoing()
            .into_iter()
            .find_map(|message| match message {
                HostMessage::Ack { id: acked, result } if acked == id => Some(result.map(drop)),
                _ => None,
            });
        assert_eq!(acked, Some(Ok(())), "Pause is acknowledged");
    }

    fn activity(&mut self) -> tcode_protocol::SessionActivity {
        self.state.deliver(
            &mut self.cx,
            90,
            tcode_protocol::ClientPayload::Subscribe(tcode_protocol::Subscription {
                topic: tcode_protocol::Topic::Index,
                after: None,
                known_index: None,
            }),
        );
        self.cx
            .drain_outgoing()
            .into_iter()
            .find_map(|message| match message {
                HostMessage::Event(EventEnvelope {
                    event: ServerEvent::IndexSnapshot(index),
                    ..
                }) => index.summary.activity.get("thread").cloned(),
                _ => None,
            })
            .expect("the index reports thread activity")
    }

    /// The user's next turn, which resumes the paused watches and is left running. A real
    /// agent is still on that turn when the catch-up read lands, where the scripted one would
    /// already have finished it, so the read waits until the turn runs.
    fn resume(&mut self) {
        let (release, gate) = mpsc::channel();
        self.host().gate = Some(gate);
        self.state.update(&mut self.cx, |state, cx| {
            state.select_session("thread", cx);
        });
        self.state.dispatch_command(
            &mut self.cx,
            80,
            Command::SendTurn {
                session_id: "thread".into(),
                text: "Continue.".into(),
                attachment_paths: Vec::new(),
            },
        );
        let (delivery_id, _) = self.delivered();
        self.events
            .try_send(AgentEvent::TurnAccepted { delivery_id })
            .unwrap();
        self.cx.run_until(|state| {
            state.resident("thread").is_some_and(|session| {
                session.delivery_in_flight.is_none() && session.turn_in_flight
            })
        });
        drop(release);
        self.cx
            .run_until(|state| !state.pull_request_watches.passing);
    }

    /// Hold the next pull request read and start a pass; send on the returned channel to answer it.
    fn hold_pass(&mut self) -> mpsc::Sender<()> {
        let (release, gate) = mpsc::channel();
        self.host().gate = Some(gate);
        self.state.update(&mut self.cx, |state, cx| {
            state.sweep_pull_request_watches(cx).detach()
        });
        let host = self.host.clone();
        self.cx
            .run_until(|_| host.lock().unwrap().requests.contains(&DETAIL.to_owned()));
        release
    }

    fn notices(&mut self) -> Vec<WatchNotice> {
        self.cx
            .drain_outgoing()
            .into_iter()
            .filter_map(|message| match message {
                HostMessage::Event(EventEnvelope {
                    event:
                        ServerEvent::Runtime(RuntimeNotification::Toast(
                            RuntimeToast::PullRequestWatch { notice, .. },
                        )),
                    ..
                }) => Some(notice),
                _ => None,
            })
            .collect()
    }
}

impl Default for Host {
    fn default() -> Self {
        Self {
            state: "OPEN",
            head: "1111111aaaa",
            checks: Vec::new(),
            protected: Vec::new(),
            ruled: Vec::new(),
            mergeable: "MERGEABLE",
            comments: Vec::new(),
            endless_comments: false,
            threads: Vec::new(),
            fingerprints: true,
            rate_limited: None,
            failing: None,
            gate: None,
            requests: Vec::new(),
        }
    }
}

fn key() -> PullRequestKey {
    PullRequestKey::new("github.com", "sample/project", 1)
}

fn snapshot(state: PullRequestState) -> PullRequestSnapshot {
    PullRequestSnapshot {
        state,
        title: "Change".into(),
        head_branch: "change".into(),
        base_branch: "main".into(),
        is_draft: false,
        updated_at: "2026-01-01T00:00:00Z".into(),
        synced_at: 1,
        closed_at: None,
        merged_at: None,
        author: None,
        additions: Some(0),
        deletions: Some(0),
        changed_files: Some(0),
        review_decision: None,
        checks_state: None,
        mergeability: Default::default(),
    }
}

#[test]
fn rate_limited_passes_wake_nobody_and_end_nothing() {
    let mut harness = Harness::new("tcode-watch-rate-limit");
    harness.host().checks = vec![check("build", "FAILURE")];
    harness.host().rate_limited = Some(DETAIL);
    harness.watch_thread("thread", START);
    for _ in 0..20 {
        harness.pass();
    }
    let requests = harness.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|operation| *operation == FINGERPRINTS)
            .count(),
        1,
        "the host's pause skips later passes without a request: {requests:?}"
    );
    let watch = harness.watch("thread").unwrap();
    assert!(watch.pending_wake.is_none() && watch.failed_checks.is_empty());
    assert!(
        harness
            .state
            .read(|state| state.pull_request_watches.failures.is_empty()),
        "a rate limit never counts toward giving up"
    );
    assert!(harness.commands.is_empty());
    assert!(harness.notices().is_empty());
}

#[test]
fn eight_failed_reads_end_the_watch_with_a_notice_delivered_before_it_ends() {
    let mut harness = Harness::new("tcode-watch-unreadable");
    harness.host().failing = Some(DETAIL);
    harness.watch_thread("thread", START);
    for _ in 0..7 {
        harness.pass();
    }
    assert!(harness.watch("thread").unwrap().pending_wake.is_none());
    harness.pass();
    let ending = harness.watch("thread").unwrap().pending_wake.unwrap();
    assert!(ending.last);
    let (delivery, text) = harness.delivered();
    assert_eq!(
        text,
        "Tcode stopped watching pull request #1 (https://github.com/sample/project/pull/1) because it failed to read it from the host 8 times in a row. Check it yourself, and call watch_pull_request to watch it again."
    );
    assert_eq!(harness.notices(), [WatchNotice::Unreadable]);
    harness.requests();
    harness.pass();
    assert!(
        harness.requests().is_empty(),
        "an ending watch is not read again"
    );
    harness.accept(delivery);
    assert_eq!(harness.watch("thread"), None);
}

#[test]
fn a_watch_whose_first_pass_finds_its_pull_request_closed_or_merged_wakes_once() {
    for (state, text, notice) in [
        (
            "CLOSED",
            "Pull request #1 (https://github.com/sample/project/pull/1) was closed, so Tcode stopped watching it. Call watch_pull_request if it reopens.",
            WatchNotice::Closed,
        ),
        (
            "MERGED",
            "Pull request #1 (https://github.com/sample/project/pull/1) was merged, so Tcode stopped watching it.",
            WatchNotice::Merged,
        ),
    ] {
        let mut harness = Harness::new("tcode-watch-terminal");
        harness.host().state = state;
        harness.watch_thread("thread", START);
        harness.pass();
        let (delivery, delivered) = harness.delivered();
        assert_eq!(delivered, text);
        assert_eq!(harness.notices(), [notice]);
        harness.accept(delivery);
        assert_eq!(harness.watch("thread"), None);
        harness.pass();
        assert!(harness.commands.is_empty(), "{state} wakes the thread once");
    }
}

#[test]
fn a_merge_the_sync_records_wakes_the_watching_thread_once_and_ends_the_watch() {
    let mut harness = Harness::new("tcode-watch-merged");
    harness.watch_thread("thread", START);
    harness.pass();
    assert!(harness.commands.is_empty());
    harness.host().state = "MERGED";
    harness.state.update(&mut harness.cx, |state, cx| {
        let mut meta = state.find_meta("thread").unwrap();
        meta.pull_requests[0].snapshot = Some(snapshot(PullRequestState::Merged));
        state.save_pull_request_meta(meta, cx);
    });
    harness.notices();
    harness.pass();
    let (delivery, text) = harness.delivered();
    assert_eq!(
        text,
        "Pull request #1 (https://github.com/sample/project/pull/1) was merged, so Tcode stopped watching it."
    );
    assert_eq!(harness.notices(), [WatchNotice::Merged]);
    harness.accept(delivery);
    assert_eq!(harness.watch("thread"), None);
    harness.pass();
    assert!(
        harness.commands.is_empty(),
        "the merge wakes the thread once"
    );
}

/// GitHub creates a job that waits on others only once they finish, so a check main requires can
/// be missing from the head while every check reported so far has passed.
#[test]
fn checks_pass_only_once_every_check_main_requires_has_reported() {
    let mut harness = Harness::new("tcode-watch-required");
    harness.host().protected = vec!["Web", "Linux x64"];
    harness.host().ruled = vec!["macOS arm64"];
    harness.host().checks = vec![
        required("Web", "SUCCESS"),
        check("Linux x64 / tests", "PENDING"),
        check("macOS arm64 / tests", "PENDING"),
    ];
    harness.watch_thread("thread", START);
    harness.pass();
    assert!(
        harness.commands.is_empty(),
        "Linux x64, which the branch protection requires, has not reported"
    );
    harness.host().checks[1] = check("Linux x64 / tests", "SUCCESS");
    harness.host().checks.push(required("Linux x64", "SUCCESS"));
    harness.pass();
    assert!(
        harness.commands.is_empty(),
        "macOS arm64, which the ruleset requires, has not reported"
    );

    harness.host().checks[2] = check("macOS arm64 / tests", "SUCCESS");
    harness
        .host()
        .checks
        .push(required("macOS arm64", "SUCCESS"));
    harness.pass();
    let (delivery, text) = harness.delivered();
    assert!(
        text.contains("- All 3 required checks passed on 1111111."),
        "{text}"
    );
    harness.accept(delivery);
    harness.pass();
    assert!(harness.commands.is_empty(), "the pass is told once");
}

#[test]
fn two_threads_share_one_read_with_separate_watermarks() {
    let mut harness = Harness::new("tcode-watch-shared");
    harness.host().comments = vec![comment("c1", "reviewer", "2026-01-01T00:05:00Z")];
    harness.watch_thread("early", START);
    harness.watch_thread("late", "2026-01-01T00:10:00.000Z");
    harness.pass();
    let requests = harness.requests();
    for operation in [FINGERPRINTS, DETAIL, ACTIVITY] {
        assert_eq!(
            requests.iter().filter(|sent| *sent == operation).count(),
            1,
            "{operation} once for both threads: {requests:?}"
        );
    }
    let early = harness.watch("early").unwrap();
    assert!(early.pending_wake.is_some());
    assert_eq!(early.remark_ids, ["c1"]);
    let late = harness.watch("late").unwrap();
    assert!(
        late.pending_wake.is_none(),
        "a remark from before its start is old news to the later thread"
    );
    assert_eq!(late.remarks_through, "2026-01-01T00:10:00.000Z");
}

#[test]
fn reads_follow_the_fingerprint_and_its_half_hour_backstop() {
    let mut harness = Harness::new("tcode-watch-fingerprint");
    harness.watch_thread("thread", START);
    harness.pass();
    harness.requests();
    harness.pass();
    assert_eq!(
        harness.requests(),
        [FINGERPRINTS],
        "a quiet pass makes only the fingerprint request"
    );
    harness.host().checks = vec![check("build", "SUCCESS"), check("lint", "FAILURE")];
    harness.pass();
    assert_eq!(
        harness.requests(),
        [FINGERPRINTS, DETAIL],
        "moved checks need the detail alone"
    );
    let (delivery, text) = harness.delivered();
    assert!(text.contains("- Checks failed on 1111111:\n  - lint https://ci.test/lint"));
    harness.accept(delivery);
    harness.host().checks.push(check("deploy", "PENDING"));
    harness.pass();
    harness.requests();
    harness.pass();
    assert_eq!(
        harness.requests(),
        [FINGERPRINTS, DETAIL],
        "a check still running is read again though the fingerprint is quiet"
    );
    harness.state.update(&mut harness.cx, |state, _| {
        for read in state.pull_request_watches.last_reads.values_mut() {
            read.at -= FINGERPRINT_REREAD;
        }
    });
    harness.pass();
    let requests = harness.requests();
    assert!(
        requests.contains(&ACTIVITY.to_owned()),
        "half an hour after the last activity read it is read again: {requests:?}"
    );
}

/// GitHub still computing mergeability keeps the pull request read every pass for ten minutes
/// at one head, and a push starts the ten minutes over.
#[test]
fn pending_mergeability_is_read_every_pass_for_ten_minutes_at_one_head() {
    let mut harness = Harness::new("tcode-watch-pending");
    harness.host().mergeable = "UNKNOWN";
    harness.watch_thread("thread", START);
    harness.pass();
    harness.requests();
    harness.pass();
    assert_eq!(harness.requests(), [FINGERPRINTS, DETAIL]);
    let age = |harness: &mut Harness| {
        harness.state.update(&mut harness.cx, |state, _| {
            for read in state.pull_request_watches.last_reads.values_mut() {
                if let Some((_, since)) = &mut read.pending {
                    *since -= PENDING_MERGEABILITY;
                }
            }
        })
    };
    age(&mut harness);
    harness.pass();
    assert_eq!(
        harness.requests(),
        [FINGERPRINTS],
        "ten minutes on, a quiet pass makes only the fingerprint request"
    );
    harness.host().head = "2222222bbbb";
    harness.pass();
    harness.requests();
    harness.pass();
    assert_eq!(harness.requests(), [FINGERPRINTS, DETAIL], "a new head");
}

/// Mergeability a host cannot work out, such as a Forgejo draft's or Bitbucket's without
/// `/conflicts`, stays unknown however often it is read, so it keeps nothing in flight; one the
/// host is still working out does.
#[test]
fn unknown_mergeability_keeps_the_usual_cadence() {
    let mut harness = Harness::new("tcode-watch-unknown");
    let group = || Group {
        key: (Some("sample".into()), key()),
        targets: Vec::new(),
        snapshot: String::new(),
    };
    let fingerprint = Fingerprint {
        status: "open".into(),
        remarks: "none".into(),
    };
    for (mergeability, read_again) in [
        (pull_request::Mergeability::Pending, true),
        (pull_request::Mergeability::Unknown, false),
    ] {
        let detail = PullRequestWatchRead {
            state: PullRequestState::Open,
            head_sha: Some("1111111aaaa".into()),
            base_branch: "main".into(),
            checks: Vec::new(),
            mergeability,
            viewer: None,
            author: None,
        };
        let full = Plan {
            detail: true,
            activity: true,
            reread_tails: false,
        };
        let next = harness.state.update(&mut harness.cx, |state, cx| {
            state.finish_pull_request_watch_read(
                group(),
                Some(fingerprint.clone()),
                full,
                Ok((detail, Some(Some(Vec::new())))),
                cx,
            );
            let last = state.pull_request_watches.last_reads.get(&group().key);
            plan(&group(), last, Some(&fingerprint), Instant::now())
        });
        assert_eq!(next.detail, read_again, "{mergeability:?}");
    }
}

#[test]
fn without_a_fingerprint_the_snapshot_gates_reads_with_a_ten_minute_backstop() {
    let mut harness = Harness::new("tcode-watch-snapshot");
    harness.host().fingerprints = false;
    harness.watch_thread("thread", START);
    harness.pass();
    harness.requests();
    harness.pass();
    assert_eq!(harness.requests(), [FINGERPRINTS]);
    harness.state.update(&mut harness.cx, |state, _| {
        for read in state.pull_request_watches.last_reads.values_mut() {
            read.at -= QUIET_REREAD;
        }
    });
    harness.pass();
    let requests = harness.requests();
    assert!(
        requests.contains(&DETAIL.to_owned()) && requests.contains(&ACTIVITY.to_owned()),
        "{requests:?}"
    );
}

#[test]
fn a_wake_reaches_a_non_resident_thread_and_a_restart_before_acceptance_redelivers_it() {
    let mut harness = Harness::new("tcode-watch-redeliver");
    harness.host().checks = vec![check("build", "FAILURE")];
    harness.watch_thread("thread", START);
    assert!(
        harness
            .state
            .read(|state| state.resident("thread").is_none())
    );
    harness.pass();
    let pending = harness.watch("thread").unwrap().pending_wake.unwrap();
    let (_, text) = harness.delivered();
    assert_eq!(text, pending.text);
    assert!(text.starts_with(
        "Update on pull request #1 (https://github.com/sample/project/pull/1), which Tcode is watching for you:\n- Checks failed on 1111111:"
    ));
    assert_eq!(
        harness.store.load_index().unwrap()[0].pull_requests[0]
            .watch
            .as_ref()
            .unwrap()
            .failed_checks,
        ["build"],
        "the watermark is written with the wake"
    );

    let mut harness = harness.restart();
    harness.requests();
    harness.pass();
    let (delivery, text) = harness.delivered();
    assert_eq!(text, pending.text, "the same wake, once");
    assert!(
        !harness.host().requests.contains(&DETAIL.to_owned()),
        "nothing is read while the last news waits for its turn"
    );
    harness.accept(delivery);
    let watch = harness.watch("thread").unwrap();
    assert_eq!(watch.pending_wake, None);
    let recorded = harness
        .store
        .read_events("thread")
        .unwrap()
        .into_iter()
        .find(|record| {
            matches!(
                record.event,
                AgentEvent::ItemCompleted(ThreadItem {
                    content: ItemContent::UserMessage { .. },
                    ..
                })
            )
        })
        .unwrap();
    assert_eq!(
        recorded.origin,
        Some(MessageOrigin::Server),
        "a wake is never a human message"
    );
    harness.pass();
    assert!(
        harness.commands.is_empty(),
        "a delivered wake is not repeated"
    );
}

#[test]
fn a_wake_steers_into_a_running_turn_and_is_not_repeated() {
    let mut harness = Harness::new("tcode-watch-steer");
    harness.host().checks = vec![check("build", "FAILURE")];
    harness.watch_thread("thread", START);
    harness.state.update(&mut harness.cx, |state, cx| {
        state.select_session("thread", cx);
    });
    harness.state.dispatch_command(
        &mut harness.cx,
        1,
        Command::SendTurn {
            session_id: "thread".into(),
            text: "Open a pull request and watch it.".into(),
            attachment_paths: Vec::new(),
        },
    );
    let (delivery_id, _) = harness.delivered();
    harness
        .events
        .try_send(AgentEvent::TurnAccepted { delivery_id })
        .unwrap();
    harness.cx.run_until(|state| {
        state
            .resident("thread")
            .is_some_and(|session| session.delivery_in_flight.is_none() && session.turn_in_flight)
    });

    harness.pass();
    let commands = harness.commands.clone();
    harness.cx.run_until(|_| !commands.is_empty());
    let (request_id, text) = match commands.try_recv() {
        Ok(SessionCommand::Steer {
            request_id, text, ..
        }) => (request_id, text),
        other => panic!("a wake during a turn steers into it, got {other:?}"),
    };
    assert!(text.starts_with(
        "Update on pull request #1 (https://github.com/sample/project/pull/1), which Tcode is watching for you:\n- Checks failed on 1111111:"
    ));
    assert_eq!(
        harness.watch("thread").unwrap().pending_wake,
        None,
        "a steered wake is delivered"
    );
    assert!(
        harness
            .state
            .read(|state| state.resident("thread").unwrap().queue.is_empty()),
        "a steered wake does not also wait in the queue"
    );
    let recorded = harness
        .store
        .read_events("thread")
        .unwrap()
        .into_iter()
        .find(|record| {
            matches!(&record.event, AgentEvent::SteerRequested { request_id: id, .. } if id == &request_id)
        })
        .expect("the steer is recorded before delivery");
    assert_eq!(
        recorded.origin,
        Some(MessageOrigin::Server),
        "a wake is never a human message"
    );

    harness.requests();
    harness.pass();
    assert!(
        harness.commands.is_empty(),
        "a delivered wake is not repeated"
    );
}

#[test]
fn pausing_an_idle_watch_and_resume_catches_up_once_after_restart() {
    for change in [
        "FAILURE",
        "SUCCESS",
        "comment",
        "conflict",
        "CLOSED",
        "MERGED",
        "unchanged",
    ] {
        let mut harness = Harness::new("tcode-watch-pause-resume");
        harness.host().checks = vec![check("build", "PENDING")];
        harness.watch_thread("thread", START);
        harness.pass();
        let baseline = harness.watch("thread").unwrap();
        assert!(harness.activity().waiting);
        harness.pause(1);
        assert_eq!(harness.watch("thread"), Some(baseline.clone()));
        let activity = harness.activity();
        assert!(
            !activity.waiting && !activity.working,
            "paused monitoring is idle"
        );
        {
            let mut host = harness.host();
            match change {
                "FAILURE" | "SUCCESS" => host.checks = vec![check("build", change)],
                "comment" => {
                    host.comments = vec![comment("c1", "reviewer", "2026-01-01T00:05:00Z")]
                }
                "conflict" => host.mergeable = "CONFLICTING",
                "CLOSED" | "MERGED" => host.state = change,
                _ => {}
            }
        }
        let mut harness = harness.restart();
        assert!(harness.state.read(|state| {
            state
                .find_meta("thread")
                .unwrap()
                .pull_request_watches_paused
        }));
        harness.requests();
        harness.pass();
        assert!(
            harness.requests().is_empty(),
            "paused watches make no reads"
        );
        assert!(
            harness.commands.is_empty(),
            "a restart cannot wake a paused thread"
        );
        assert_eq!(harness.watch("thread"), Some(baseline));
        assert!(!harness.activity().waiting);

        harness.resume();
        if change == "unchanged" {
            assert!(harness.commands.is_empty(), "resume without news is silent");
            assert!(harness.notices().is_empty());
        } else {
            let text = harness.steered();
            let expected = match change {
                "FAILURE" => "Checks failed",
                "SUCCESS" => "passed",
                "comment" => "note c1",
                "conflict" => "conflicts with main",
                "CLOSED" => "was closed",
                "MERGED" => "was merged",
                _ => unreachable!(),
            };
            assert!(text.contains(expected), "{change}: {text}");
            assert_eq!(harness.notices().len(), 1, "one catch-up per PR");
        }
        harness.finish_turn();
        harness.pass();
        assert!(
            harness.commands.is_empty(),
            "unchanged news is never repeated"
        );
        assert!(harness.notices().is_empty());
    }
}

#[test]
fn pausing_blocks_an_in_flight_read_and_accepts_agent_watches_while_paused() {
    let mut harness = Harness::new("tcode-watch-paused-registration");
    harness.host().checks = vec![check("build", "FAILURE")];
    harness.watch_thread("thread", START);
    let baseline = harness.watch("thread").unwrap();
    let release = harness.hold_pass();
    harness.pause(1);
    release.send(()).unwrap();
    harness
        .cx
        .run_until(|state| !state.pull_request_watches.passing);
    assert_eq!(harness.watch("thread"), Some(baseline));
    assert!(harness.commands.is_empty());
    assert!(harness.notices().is_empty());

    harness.state.dispatch_command(
        &mut harness.cx,
        2,
        Command::WatchPullRequest {
            session_id: "thread".into(),
            key: key(),
            watching: false,
        },
    );
    assert!(harness.watch("thread").is_none());
    let (reply, answer) = async_channel::bounded(1);
    harness.state.update(&mut harness.cx, |state, cx| {
        state.mcp.pull_request_registrations.insert(
            "thread".into(),
            crate::app::orchestrate::PullRequestRegistration {
                registration: agent::McpRegistration {
                    name: "tcode_pull_requests".into(),
                    url: "http://127.0.0.1/pull-requests".into(),
                    bearer_token: "token".into(),
                },
                hosts: "GitHub".into(),
                instructions: String::new(),
            },
        );
        state.handle_pull_request_request(
            pull_request_mcp::BrokerRequest {
                session_id: "thread".into(),
                operation: pull_request_mcp::Operation::Watch(pull_request_mcp::Target {
                    url: Some("https://github.com/sample/project/pull/1".into()),
                    repository: None,
                    number: None,
                    host: None,
                }),
                reply,
            },
            cx,
        );
    });
    harness.cx.run_until(|_| !answer.is_empty());
    let registered = answer.try_recv().unwrap().unwrap();
    assert_eq!(registered["watching"], true);
    assert_eq!(registered["paused"], true);
    let registered = harness.watch("thread").unwrap();
    harness.pass();
    assert!(
        harness.commands.is_empty(),
        "new registrations inherit the pause"
    );
    harness.resume();
    let text = harness.steered();
    assert!(text.contains("Checks failed"));
    assert_eq!(
        harness.watch("thread").unwrap().started_at,
        registered.started_at
    );
    harness.finish_turn();
    harness.pass();
    assert!(harness.commands.is_empty());
}

#[test]
fn a_late_turn_acceptance_cannot_resume_watches_after_interrupt() {
    let mut harness = Harness::new("tcode-watch-late-acceptance");
    harness.watch_thread("thread", START);
    harness.state.update(&mut harness.cx, |state, cx| {
        state.select_session("thread", cx);
    });
    harness.state.dispatch_command(
        &mut harness.cx,
        1,
        Command::SendTurn {
            session_id: "thread".into(),
            text: "Watch this PR.".into(),
            attachment_paths: Vec::new(),
        },
    );
    let (delivery, _) = harness.delivered();
    assert_eq!(harness.stop(2), Ok(()));
    assert!(matches!(
        harness.commands.try_recv(),
        Ok(SessionCommand::Interrupt)
    ));
    harness.host().checks = vec![check("build", "FAILURE")];
    harness.accept(delivery);
    harness.pass();
    assert!(!harness.activity().waiting);
    assert!(
        harness.commands.is_empty(),
        "accepting a pre-Stop turn cannot resume monitoring"
    );
    harness.resume();
    assert!(harness.steered().contains("Checks failed"));
    harness.finish_turn();
}

#[test]
fn resume_combines_pending_news_with_changes_from_the_pause() {
    let mut harness = Harness::new("tcode-watch-pending-catchup");
    harness.watch_thread("thread", START);
    harness.host().checks = vec![check("build", "FAILURE")];
    harness.pass();
    let (_, pending) = harness.delivered();
    assert!(pending.contains("Checks failed"));
    assert_eq!(harness.stop(1), Ok(()));
    let mut harness = harness.restart();
    harness.host().checks = vec![check("build", "SUCCESS")];
    harness.host().comments = vec![comment("paused", "reviewer", "2026-01-01T00:05:00Z")];
    harness.requests();
    harness.pass();
    assert!(harness.requests().is_empty());
    harness.resume();
    let text = harness.steered();
    assert!(text.contains("Checks failed"), "undelivered news survives");
    assert!(
        text.contains("passed") && text.contains("note paused"),
        "resume refreshes despite pending news: {text}"
    );
    harness.finish_turn();
    harness.pass();
    assert!(
        harness.commands.is_empty(),
        "combined news is delivered once"
    );
}

#[test]
fn late_acceptance_of_old_news_delivers_only_the_remaining_catchup() {
    let mut harness = Harness::new("tcode-watch-live-pending");
    harness.watch_thread("thread", START);
    harness.host().checks = vec![check("build", "FAILURE")];
    harness.pass();
    let (old_delivery, old_text) = harness.delivered();
    assert!(old_text.contains("Checks failed"));
    assert_eq!(harness.stop(1), Ok(()));
    assert!(matches!(
        harness.commands.try_recv(),
        Ok(SessionCommand::Interrupt)
    ));
    harness.host().checks = vec![check("build", "SUCCESS")];
    harness.state.update(&mut harness.cx, |state, cx| {
        state.select_session("thread", cx);
    });
    harness.state.dispatch_command(
        &mut harness.cx,
        2,
        Command::SendTurn {
            session_id: "thread".into(),
            text: "Continue.".into(),
            attachment_paths: Vec::new(),
        },
    );
    harness.cx.run_until(|state| {
        state.find_meta("thread").unwrap().pull_requests[0]
            .watch
            .as_ref()
            .and_then(|watch| watch.pending_wake.as_ref())
            .is_some_and(|wake| wake.text.contains("passed"))
    });
    harness.accept(old_delivery);
    let text = match harness.commands.try_recv() {
        Ok(SessionCommand::Steer { text, .. }) => text,
        other => panic!("catch-up steers into the accepted turn: {other:?}"),
    };
    assert!(text.contains("passed"));
    assert!(
        !text.contains("Checks failed"),
        "acknowledged news is not repeated"
    );
    let (human_delivery, text) = harness.delivered();
    assert_eq!(text, "Continue.");
    harness.accept(human_delivery);
    harness.pass();
    assert!(harness.commands.is_empty());
}

#[test]
fn ending_paused_watches_prevents_a_later_turn_from_restoring_them() {
    for command in [
        Command::SettleSession {
            session_id: "thread".into(),
        },
        Command::ArchiveSession {
            session_id: "thread".into(),
        },
        Command::WatchPullRequest {
            session_id: "thread".into(),
            key: key(),
            watching: false,
        },
    ] {
        let mut harness = Harness::new("tcode-watch-end-paused");
        harness.watch_thread("thread", START);
        harness.pause(1);
        let archived = matches!(command, Command::ArchiveSession { .. });
        harness.state.dispatch_command(&mut harness.cx, 2, command);
        harness.cx.run_until_parked();
        assert!(harness.watch("thread").is_none());
        if archived {
            harness.state.dispatch_command(
                &mut harness.cx,
                3,
                Command::UnarchiveSession {
                    session_id: "thread".into(),
                },
            );
        }
        harness.state.dispatch_command(
            &mut harness.cx,
            4,
            Command::UnsettleSession {
                session_id: "thread".into(),
            },
        );
        harness.resume();
        harness.pass();
        assert!(harness.watch("thread").is_none());
        assert!(harness.commands.is_empty());
    }
}

#[test]
fn incomplete_remarks_never_advance_the_watermark_but_checks_still_report() {
    let mut harness = Harness::new("tcode-watch-incomplete");
    harness.host().checks = vec![check("build", "FAILURE")];
    let mut replies: Vec<_> = (0..12)
        .map(|index| comment(&format!("r{index}"), "tcode-bot", "2026-01-01T00:01:00Z"))
        .collect();
    replies[11] = comment("r11", "reviewer", "2026-01-01T00:02:00Z");
    harness.host().threads = vec![ReviewThread {
        id: "thread-a",
        comments: replies,
    }];
    harness.host().failing = Some(TAIL);
    harness.watch_thread("thread", START);
    harness.pass();
    let watch = harness.watch("thread").unwrap();
    assert_eq!(watch.failed_checks, ["build"], "the failure is still told");
    assert_eq!(watch.remarks_through, START);
    let (delivery, text) = harness.delivered();
    assert!(!text.contains("new comment"));
    harness.accept(delivery);

    harness.host().failing = None;
    harness.pass();
    let (_, text) = harness.delivered();
    assert!(
        text.contains("- 1 new comment:\n  - reviewer on src/lib.rs: \"note r11\""),
        "the reply past the first page wakes once it can be read: {text}"
    );
}

#[test]
fn a_list_past_ten_pages_is_incomplete() {
    let mut harness = Harness::new("tcode-watch-pages");
    harness.host().checks = vec![check("build", "FAILURE")];
    harness.host().endless_comments = true;
    harness.host().comments = vec![comment("c1", "reviewer", "2026-01-01T00:05:00Z")];
    harness.watch_thread("thread", START);
    harness.pass();
    let requests = harness.requests();
    assert_eq!(
        requests.iter().filter(|sent| *sent == ACTIVITY).count(),
        10,
        "{requests:?}"
    );
    let watch = harness.watch("thread").unwrap();
    assert_eq!(
        watch.remarks_through, START,
        "the comment read so far is not news yet"
    );
    assert_eq!(watch.failed_checks, ["build"]);
    let (_, text) = harness.delivered();
    assert!(text.contains("- Checks failed") && !text.contains("new comment"));
}

#[test]
fn the_half_hour_reread_pages_cached_review_thread_tails_again() {
    let mut harness = Harness::new("tcode-watch-tails");
    let replies: Vec<_> = (0..12)
        .map(|index| comment(&format!("r{index}"), "tcode-bot", "2026-01-01T00:01:00Z"))
        .collect();
    harness.host().threads = vec![ReviewThread {
        id: "thread-a",
        comments: replies,
    }];
    harness.watch_thread("thread", START);
    harness.pass();
    assert!(harness.watch("thread").unwrap().pending_wake.is_none());
    harness.host().threads[0].comments[11] = {
        let mut edited = comment("r11", "reviewer", "2026-01-01T00:01:00Z");
        edited["lastEditedAt"] = json!("2026-01-01T00:20:00Z");
        edited
    };
    harness.state.update(&mut harness.cx, |state, _| {
        for read in state.pull_request_watches.last_reads.values_mut() {
            read.at -= FINGERPRINT_REREAD;
        }
    });
    harness.requests();
    harness.pass();
    assert!(harness.requests().contains(&TAIL.to_owned()));
    let (_, text) = harness.delivered();
    assert!(
        text.contains("reviewer on src/lib.rs: \"note r11\""),
        "an edit past a thread's first page is seen though its count stayed: {text}"
    );
}

#[test]
fn agent_starts_are_refused_where_the_parent_owns_the_pull_request_or_it_merged() {
    let mut harness = Harness::new("tcode-watch-refusals");
    harness.watch_thread("thread", START);
    let start = |harness: &mut Harness, edit: &dyn Fn(&mut SessionMeta)| {
        harness.state.update(&mut harness.cx, |state, cx| {
            let mut meta = state.find_meta("thread").unwrap();
            meta.pull_requests[0].watch = None;
            edit(&mut meta);
            state.save_pull_request_meta(meta, cx);
            state.watch_pull_request_from_agent(
                "thread",
                key(),
                "https://github.com/sample/project/pull/1".into(),
                true,
                cx,
            )
        })
    };
    for (edit, refusal) in [
        (
            &(|meta: &mut SessionMeta| meta.parent_session_id = Some("lead".into()))
                as &dyn Fn(&mut SessionMeta),
            "Its parent thread owns the pull request",
        ),
        (
            &|meta: &mut SessionMeta| meta.settled_at = Some(1),
            "Unsettle",
        ),
        (
            &|meta: &mut SessionMeta| meta.archived_at = Some(1),
            "archived",
        ),
        (
            &|meta: &mut SessionMeta| {
                meta.pull_requests[0].snapshot = Some(snapshot(PullRequestState::Merged))
            },
            "merged",
        ),
    ] {
        let error = start(&mut harness, edit).unwrap_err();
        assert!(error.contains(refusal), "{error}");
        harness.state.update(&mut harness.cx, |state, cx| {
            let mut meta = state.find_meta("thread").unwrap();
            meta.parent_session_id = None;
            meta.settled_at = None;
            meta.archived_at = None;
            meta.pull_requests[0].snapshot = None;
            state.save_pull_request_meta(meta, cx);
        });
    }
    let started = start(&mut harness, &|meta| {
        meta.pull_requests[0].snapshot = Some(snapshot(PullRequestState::Closed))
    })
    .unwrap();
    assert_eq!(
        started["watching"], true,
        "a saved closed PR may have reopened"
    );
    assert_eq!(started["wasWatching"], false);

    harness.state.update(&mut harness.cx, |state, cx| {
        state.settle_session("thread", cx)
    });
    assert_eq!(harness.watch("thread"), None, "settling ends the watch");
    harness.state.update(&mut harness.cx, |state, cx| {
        state.unsettle_session("thread", cx)
    });
    assert_eq!(
        harness.watch("thread"),
        None,
        "un-settling never restarts it"
    );
}
