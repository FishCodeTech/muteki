use super::*;
use agent::{AgentEvent, ItemContent, ProviderKind, SessionCommand, ThreadItem, TurnStatus};
use std::path::PathBuf;
use tcode_core::project::{Project, SessionMeta};
use tcode_core::session::Author;
use tcode_core::settings::{EnvVar, ProviderProfile, ProviderSettings, Settings};
use tcode_protocol::{IndexSnapshot, Scope};
use tcode_services::settings::SettingsStore;
use tcode_services::store::Mutation;

struct SpaceHost {
    host: SpawnedHost,
    store: SessionStore,
    a: Project,
    b: Project,
    next_id: u64,
    events: Vec<EventEnvelope>,
    provider: crate::app::ScriptedProvider,
}

impl SpaceHost {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("tcode-spaces-{}", uuid::Uuid::new_v4()));
        let a = Project::from_root(root.join("A"));
        let b = Project::from_root(root.join("B"));
        std::fs::create_dir_all(&a.root).unwrap();
        std::fs::create_dir_all(&b.root).unwrap();
        std::fs::write(a.root.join("visible.txt"), b"A").unwrap();
        std::fs::write(b.root.join("private.txt"), b"B").unwrap();
        let store = SessionStore::open_at(root).unwrap();
        let mut mutations = vec![
            Mutation::upsert_project(a.clone()),
            Mutation::upsert_project(b.clone()),
        ];
        for (id, project, updated_at, archived_at) in [
            ("a", &a, 10, None),
            ("b", &b, 20, None),
            ("archived-a", &a, 5, Some(5)),
            ("archived-b", &b, 6, Some(6)),
        ] {
            let mut meta = SessionMeta::new(ProviderKind::Codex, project.root.clone(), None);
            meta.id = id.into();
            meta.title = id.into();
            meta.project_id = Some(project.id.clone());
            meta.updated_at = updated_at;
            meta.archived_at = archived_at;
            meta.resume_cursor = Some(agent::ResumeCursor(serde_json::json!({"thread_id": id})));
            mutations.push(Mutation::upsert_meta(meta));
            mutations.push(
                Mutation::append_event(
                    id,
                    updated_at,
                    &AgentEvent::ItemCompleted(ThreadItem {
                        id: format!("user-{id}"),
                        parent_item_id: None,
                        content: ItemContent::UserMessage {
                            text: "needle".into(),
                            context_len: None,
                            attachments: Vec::new(),
                        },
                    }),
                )
                .unwrap(),
            );
        }
        store.apply(&mutations).unwrap();
        let mut settings = Settings {
            auto_settle_after_days: None,
            ..Settings::default()
        };
        settings.profiles.insert(
            "shared-profile".into(),
            ProviderProfile {
                kind: ProviderKind::Codex,
                settings: ProviderSettings {
                    display_name: Some("Team model".into()),
                    custom_models: vec!["team-model".into()],
                    env: vec![EnvVar {
                        name: "BASE_URL".into(),
                        value: "https://private-endpoint.invalid".into(),
                        sensitive: false,
                    }],
                    ..ProviderSettings::default()
                },
            },
        );
        SettingsStore::new(store.root().clone())
            .save(&settings)
            .unwrap();
        let host = spawn_host(store.clone(), HostServices::default()).unwrap();
        let provider = crate::app::scripted_provider(ProviderKind::Codex);
        let launcher = provider.launcher.clone();
        smol::block_on(
            host.update_state_for_test(move |state, _| {
                state.set_provider_launcher_for_test(launcher)
            }),
        )
        .unwrap();
        Self {
            host,
            store,
            a,
            b,
            next_id: 1,
            events: Vec::new(),
            provider,
        }
    }

    fn member(&self, device: &str) -> Principal {
        Principal::Space {
            policy_revision: 1,
            space_id: "space-a".into(),
            space_name: "Team A".into(),
            project_ids: vec![self.a.id.clone()],
            device_id: device.into(),
            device_name: format!("Member {device}"),
        }
    }

    fn recv(&mut self) -> HostMessage {
        let line = smol::block_on(smol::future::race(
            async { self.host.from_host.recv().await.unwrap() },
            async {
                smol::Timer::after(std::time::Duration::from_secs(10)).await;
                panic!("host reply timeout")
            },
        ));
        let message = tcode_protocol::decode_host_line(&line).unwrap();
        if let HostMessage::Event(event) = &message {
            self.events.push(event.clone());
        }
        message
    }

    fn request(&mut self, principal: Principal, payload: ClientPayload) -> HostMessage {
        let id = self.next_id;
        self.next_id += 1;
        self.host
            .to_host
            .send_blocking(
                tcode_protocol::encode_line(&ClientMessage {
                    id,
                    key: None,
                    principal: Some(principal),
                    payload,
                })
                .unwrap(),
            )
            .unwrap();
        loop {
            let message = self.recv();
            if matches!(&message, HostMessage::Ack { id: reply, .. } | HostMessage::QueryResult { id: reply, .. } if *reply == id)
            {
                return message;
            }
        }
    }

    fn command(&mut self, principal: Principal, command: Command) -> CommandResponse {
        match self.request(principal, ClientPayload::Command(command)) {
            HostMessage::Ack {
                result: Ok(response),
                ..
            } => response,
            reply => panic!("command failed: {reply:?}"),
        }
    }

    fn query(&mut self, principal: Principal, query: Query) -> QueryResponse {
        match self.request(principal, ClientPayload::Query(query)) {
            HostMessage::QueryResult {
                result: Ok(response),
                ..
            } => response,
            reply => panic!("query failed: {reply:?}"),
        }
    }

    fn subscribe(&mut self, principal: Principal, topic: Topic) {
        assert!(matches!(
            self.request(
                principal,
                ClientPayload::Subscribe(Subscription {
                    known_index: None,
                    topic,
                    after: None
                })
            ),
            HostMessage::Ack { result: Ok(_), .. }
        ));
    }

    fn event(&mut self, predicate: impl Fn(&EventEnvelope) -> bool) -> EventEnvelope {
        loop {
            if let Some(index) = self.events.iter().position(&predicate) {
                return self.events.remove(index);
            }
            self.recv();
        }
    }

    fn draft(&mut self, principal: Principal, project: Project, cwd: PathBuf) -> String {
        match self.command(
            principal,
            Command::StartDraft {
                project_id: project.id,
                cwd,
            },
        ) {
            CommandResponse::SessionId(Some(id)) => id,
            reply => panic!("draft response: {reply:?}"),
        }
    }

    fn finish(mut self) -> Vec<tcode_core::session::StoredEvent> {
        self.command(Principal::Full, Command::ShutdownAllAndFlush);
        let root = self.store.root().clone();
        let stopped = self.host.stopped.clone();
        drop(self.host);
        stopped.recv_blocking().unwrap();
        drop(self.store);
        let reopened = SessionStore::open_at(root.clone()).unwrap();
        let records = reopened.read_events("a").unwrap();
        reopened.close().unwrap();
        // Windows may still hold a terminal's cwd under the root for a moment
        // after shutdown; a leftover temp dir is not a test failure.
        let _ = std::fs::remove_dir_all(root);
        records
    }
}

fn denied(reply: HostMessage) {
    let error = match reply {
        HostMessage::Ack {
            result: Err(error), ..
        }
        | HostMessage::QueryResult {
            result: Err(error), ..
        } => error,
        reply => panic!("expected refusal: {reply:?}"),
    };
    assert_eq!(error.code, "out_of_scope");
    assert!(!error.message.contains('/'));
}

#[test]
fn space_index_and_scope_project_only_the_members_projects() {
    let mut host = SpaceHost::new();
    let member = host.member("one");
    let topic = Topic::SpaceIndex {
        space_id: "space-a".into(),
    };
    host.subscribe(member.clone(), topic.clone());
    let ServerEvent::IndexSnapshot(IndexSnapshot {
        projects,
        sessions,
        summary,
    }) = host
        .event(|event| event.topic == topic && event.request_id.is_some())
        .event
    else {
        panic!("index snapshot")
    };
    assert_eq!(projects, [host.a.clone()]);
    assert_eq!(
        sessions
            .iter()
            .map(|meta| meta.id.as_str())
            .collect::<Vec<_>>(),
        ["a"]
    );
    assert_eq!(summary.archived_counts.get(&host.a.id), Some(&1));
    assert_eq!(summary.archived_counts.len(), 1);
    assert_eq!(summary.activity.keys().collect::<Vec<_>>(), ["a"]);

    host.subscribe(member.clone(), Topic::Scope);
    let scope = host.event(|event| event.topic == Topic::Scope && event.request_id.is_some());
    let ServerEvent::ScopeSnapshot(Scope::Space {
        projects,
        providers,
        ..
    }) = &scope.event
    else {
        panic!("space scope")
    };
    assert_eq!(projects, &[host.a.clone()]);
    let profile = providers
        .iter()
        .find(|choice| choice.profile_id.as_deref() == Some("shared-profile"))
        .unwrap();
    assert_eq!(profile.name, "Team model");
    assert!(profile.models.iter().any(|model| model.id == "team-model"));
    let wire = serde_json::to_string(&scope).unwrap();
    assert!(!wire.contains("private-endpoint"));
    assert!(!wire.contains("BASE_URL"));
    assert!(!wire.contains("secrets"));

    host.events.clear();
    let mut stale = member.clone();
    if let Principal::Space {
        project_ids,
        policy_revision,
        ..
    } = &mut stale
    {
        project_ids.push(host.b.id.clone());
        *policy_revision = 0;
    }
    host.query(stale.clone(), Query::Ping);
    if let Principal::Space {
        policy_revision, ..
    } = &mut stale
    {
        *policy_revision = 2;
    }
    host.request(
        stale,
        ClientPayload::Unsubscribe(Subscription {
            known_index: None,
            topic: Topic::SessionStatus {
                session_id: "b".into(),
            },
            after: None,
        }),
    );
    host.command(
        Principal::Full,
        Command::RenameSession {
            session_id: "b".into(),
            title: "Private B".into(),
        },
    );
    let extra = host.b.root.join("extra");
    std::fs::create_dir_all(&extra).unwrap();
    host.command(Principal::Full, Command::CreateProject { root: extra });
    host.query(Principal::Full, Query::Ping);
    assert!(!host.events.iter().any(|event| event.topic == topic));
    assert!(!host.events.iter().any(|event| event.topic == Topic::Scope));
    let b = host.draft(Principal::Full, host.b.clone(), host.b.root.clone());
    host.command(
        Principal::Full,
        Command::ScheduleTurn {
            session_id: b,
            text: "later B".into(),
            attachment_paths: Vec::new(),
            fire_at_unix_secs: u64::from(u32::MAX),
        },
    );
    host.query(member.clone(), Query::Ping);
    assert!(!host.events.iter().any(|event| event.topic == topic));
    let a = host.draft(member.clone(), host.a.clone(), host.a.root.clone());
    host.command(
        member,
        Command::ScheduleTurn {
            session_id: a.clone(),
            text: "later A".into(),
            attachment_paths: Vec::new(),
            fire_at_unix_secs: u64::from(u32::MAX),
        },
    );
    let added = host.event(|event| {
        event.topic == topic
            && matches!(&event.event, ServerEvent::IndexUpsertSession(meta) if meta.id == a)
    });
    assert!(added.request_id.is_none());
    host.query(Principal::Full, Query::Ping);
    host.events.clear();
    host.command(
        Principal::Full,
        Command::ArchiveSession {
            session_id: "b".into(),
        },
    );
    host.query(Principal::Full, Query::Ping);
    assert!(!host.events.iter().any(|event| event.topic == topic));
    let mut revision = summary.archived_revision;
    for (command, count) in [
        (
            Command::ArchiveSession {
                session_id: "a".into(),
            },
            2,
        ),
        (
            Command::UnarchiveSession {
                session_id: "a".into(),
            },
            1,
        ),
        (
            Command::ArchiveSession {
                session_id: "a".into(),
            },
            2,
        ),
    ] {
        host.events.clear();
        host.command(Principal::Full, command);
        let project_id = host.a.id.clone();
        let ServerEvent::IndexSummaryReplaced(summary) = host.event(|event| event.topic == topic && matches!(&event.event, ServerEvent::IndexSummaryReplaced(summary) if summary.archived_counts.get(&project_id) == Some(&count))).event else { panic!("space archive summary") };
        assert!(summary.archived_revision > revision);
        revision = summary.archived_revision;
        let QueryResponse::ArchivedSessions(archive) =
            host.query(host.member("one"), Query::ArchivedSessions)
        else {
            panic!("space archive")
        };
        assert_eq!(archive.revision, revision);
    }
    host.request(
        host.member("one"),
        ClientPayload::Unsubscribe(Subscription {
            known_index: None,
            topic: topic.clone(),
            after: None,
        }),
    );
    host.subscribe(host.member("one"), topic.clone());
    let ServerEvent::IndexSnapshot(index) = host
        .event(|event| event.topic == topic && event.request_id.is_some())
        .event
    else {
        panic!("reconnected index")
    };
    assert!(index.summary.archived_revision >= revision);
    assert_eq!(index.projects, [host.a.clone()]);
    host.finish();
}

#[test]
fn space_refuses_foreign_sessions_paths_terminals_and_host_subscriptions() {
    let mut host = SpaceHost::new();
    let member = host.member("one");
    for command in [
        Command::CreateNewProject {
            name: "outside-space".into(),
        },
        Command::StartScratchDraft,
        // Authority over tcode_control is the host owner's: a space member may neither give
        // its threads a grant nor approve one their model asked for.
        Command::SetControlGrant {
            session_id: "a".into(),
            grant: Some(tcode_core::control::ControlGrant::default_grant()),
        },
        Command::RespondControlGrant {
            session_id: "a".into(),
            request_id: "request".into(),
            approve: true,
        },
        // A reference would let thread a's model read thread b, which the space cannot read.
        Command::AddThreadReference {
            session_id: "a".into(),
            thread_id: "b".into(),
        },
        // A space moves its threads only between its own projects.
        Command::MoveSession {
            session_id: "a".into(),
            project_id: host.b.id.clone(),
            cwd: None,
        },
        Command::MoveSession {
            session_id: "b".into(),
            project_id: host.a.id.clone(),
            cwd: None,
        },
    ] {
        denied(host.request(member.clone(), ClientPayload::Command(command)));
    }
    for topic in [
        Topic::Index,
        Topic::Settings,
        Topic::Providers,
        Topic::RuntimeEvents,
        Topic::Preview,
        Topic::ExternalImport {
            project_id: host.a.id.clone(),
        },
        Topic::SpaceIndex {
            space_id: "other".into(),
        },
        Topic::SessionEvents {
            session_id: "b".into(),
        },
        Topic::SessionStatus {
            session_id: "b".into(),
        },
        Topic::SessionPlan {
            session_id: "b".into(),
        },
        Topic::GitStatus {
            session_id: "b".into(),
        },
        Topic::SessionEvents {
            session_id: "unknown".into(),
        },
    ] {
        denied(host.request(
            member.clone(),
            ClientPayload::Subscribe(Subscription {
                known_index: None,
                topic,
                after: None,
            }),
        ));
    }
    denied(host.request(
        member.clone(),
        ClientPayload::Command(Command::SendTurn {
            session_id: "b".into(),
            text: "forged".into(),
            attachment_paths: Vec::new(),
        }),
    ));
    denied(host.request(
        member.clone(),
        ClientPayload::Query(Query::ReadFileBytes {
            path: host.b.root.join("private.txt"),
            max_bytes: None,
        }),
    ));
    denied(host.request(
        member.clone(),
        ClientPayload::Query(Query::ReadItemOutput {
            session_id: "a".into(),
            item_id: "foreign-tool".into(),
        }),
    ));
    for session_id in ["a", "b"] {
        denied(host.request(
            member.clone(),
            ClientPayload::Query(Query::ReadItemImage {
                session_id: session_id.into(),
                item_id: "foreign-tool".into(),
                image_index: 0,
                max_bytes: None,
            }),
        ));
    }
    assert_eq!(
        host.query(
            member.clone(),
            Query::ReadFileBytes {
                path: host.a.root.join("visible.txt"),
                max_bytes: None
            }
        ),
        QueryResponse::FileBytes(b"A".to_vec())
    );
    host.subscribe(
        member.clone(),
        Topic::SessionStatus {
            session_id: "a".into(),
        },
    );
    denied(host.request(
        member.clone(),
        ClientPayload::Command(Command::SendTurn {
            session_id: "a".into(),
            text: "attachment".into(),
            attachment_paths: vec![host.a.root.join("visible.txt")],
        }),
    ));
    denied(host.request(
        member.clone(),
        ClientPayload::Query(Query::RemoveUserFile {
            path: host.a.root.join("visible.txt"),
        }),
    ));
    let ServerEvent::SessionStatusReplaced(status) = host
        .event(|event| matches!(&event.event, ServerEvent::SessionStatusReplaced(status) if status.session_id == "a") && event.request_id.is_some())
        .event
    else {
        panic!("session status")
    };
    let QueryResponse::SavedAttachment(path) = host.query(
        member.clone(),
        Query::SaveAttachment {
            dir: status.attachments_dir,
            bytes: b"A".to_vec(),
            ext: "txt".into(),
        },
    ) else {
        panic!("saved attachment")
    };
    assert_eq!(
        host.command(
            member.clone(),
            Command::SendTurn {
                session_id: "a".into(),
                text: "attachment".into(),
                attachment_paths: vec![path],
            },
        ),
        CommandResponse::Unit
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            host.b.root.join("private.txt"),
            host.a.root.join("escape.txt"),
        )
        .unwrap();
        denied(host.request(
            member.clone(),
            ClientPayload::Query(Query::ReadFileBytes {
                path: host.a.root.join("escape.txt"),
                max_bytes: None,
            }),
        ));
    }
    host.subscribe(
        Principal::Full,
        Topic::SessionStatus {
            session_id: "b".into(),
        },
    );
    host.command(
        Principal::Full,
        Command::NewTerminal {
            session_id: "b".into(),
        },
    );
    let status = host.event(|event| matches!(&event.event, ServerEvent::SessionStatusReplaced(status) if status.session_id == "b" && !status.terminals.is_empty()));
    let ServerEvent::SessionStatusReplaced(status) = status.event else {
        unreachable!()
    };
    let terminal_id = status.terminals[0].id;
    denied(host.request(
        member.clone(),
        ClientPayload::Command(Command::TerminalInput {
            terminal_id,
            bytes: b"echo forbidden\n".to_vec(),
        }),
    ));
    denied(host.request(
        member.clone(),
        ClientPayload::Subscribe(Subscription {
            known_index: None,
            topic: Topic::Terminal { terminal_id },
            after: None,
        }),
    ));
    denied(host.request(
        member,
        ClientPayload::Command(Command::ActivateTerminal {
            session_id: "a".into(),
            terminal_id,
        }),
    ));
    host.finish();
}

#[test]
fn space_drafts_are_bound_to_workspace_and_device_and_search_filters_before_limit() {
    let mut host = SpaceHost::new();
    let one = host.member("one");
    let two = host.member("two");
    let mut both = one.clone();
    if let Principal::Space { project_ids, .. } = &mut both {
        project_ids.push(host.b.id.clone());
    }
    denied(host.request(
        both,
        ClientPayload::Command(Command::StartDraft {
            project_id: host.a.id.clone(),
            cwd: host.b.root.clone(),
        }),
    ));
    denied(host.request(
        one.clone(),
        ClientPayload::Command(Command::StartDraft {
            project_id: host.a.id.clone(),
            cwd: host.b.root.clone(),
        }),
    ));
    let a = host.draft(one.clone(), host.a.clone(), host.a.root.clone());
    let repeated = host.draft(one.clone(), host.a.clone(), host.a.root.clone());
    let b = host.draft(two, host.a.clone(), host.a.root.clone());
    assert_eq!(a, repeated);
    assert_ne!(a, b);
    host.subscribe(one.clone(), Topic::SessionStatus { session_id: a });
    let QueryResponse::SessionContentHits(owner_hits) = host.query(
        Principal::Full,
        Query::SearchSessionContent {
            query: "needle".into(),
            limit: 1,
        },
    ) else {
        panic!("owner search")
    };
    assert_eq!(owner_hits[0].session_id, "b");
    let QueryResponse::SessionContentHits(hits) = host.query(
        one.clone(),
        Query::SearchSessionContent {
            query: "needle".into(),
            limit: 1,
        },
    ) else {
        panic!("space search")
    };
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].session_id, "a");
    let QueryResponse::ArchivedSessions(archived) =
        host.query(one.clone(), Query::ArchivedSessions)
    else {
        panic!("archive query")
    };
    assert_eq!(
        archived
            .sessions
            .iter()
            .map(|meta| meta.id.as_str())
            .collect::<Vec<_>>(),
        ["archived-a"]
    );
    assert_eq!(
        host.command(
            one.clone(),
            Command::MarkSessionUnread {
                session_id: "a".into()
            }
        ),
        CommandResponse::Unit
    );
    host.command(
        one.clone(),
        Command::MarkSessionRead {
            session_id: "a".into(),
            through: u64::MAX,
        },
    );
    host.subscribe(Principal::Full, Topic::Settings);
    let ServerEvent::SettingsSnapshot(settings) = host
        .event(|event| event.topic == Topic::Settings && event.request_id.is_some())
        .event
    else {
        panic!("settings snapshot")
    };
    assert!(!settings.last_visited.contains_key("a"));
    let CommandResponse::SessionId(Some(fork)) =
        host.command(one.clone(), Command::ForkThread { id: "a".into() })
    else {
        panic!("fork response")
    };
    host.subscribe(
        one.clone(),
        Topic::SessionStatus {
            session_id: fork.clone(),
        },
    );
    host.subscribe(
        one.clone(),
        Topic::SessionEvents {
            session_id: fork.clone(),
        },
    );
    host.event(|event| {
        event.request_id.is_some()
            && matches!(&event.topic, Topic::SessionStatus { session_id } if session_id == &fork)
    });
    let ServerEvent::SessionSnapshot { records, .. } = host.event(|event| {
        event.request_id.is_some()
            && matches!(&event.topic, Topic::SessionEvents { session_id } if session_id == &fork)
    }).event else { panic!("fork history snapshot") };
    assert!(records.iter().any(|record| matches!(&record.event,
        AgentEvent::ItemCompleted(ThreadItem { content: ItemContent::UserMessage { text, .. }, .. }) if text == "needle")));
    host.finish();
}

#[test]
fn space_turns_keep_the_sending_author_through_provider_delivery_and_queueing() {
    let mut host = SpaceHost::new();
    host.subscribe(
        Principal::Full,
        Topic::SessionEvents {
            session_id: "a".into(),
        },
    );
    host.subscribe(
        Principal::Full,
        Topic::SessionStatus {
            session_id: "a".into(),
        },
    );
    for (device, text) in [("one", "first"), ("two", "queued")] {
        let member = host.member(device);
        host.command(
            member,
            Command::SendTurn {
                session_id: "a".into(),
                text: text.into(),
                attachment_paths: Vec::new(),
            },
        );
        if text == "first" {
            let SessionCommand::SendTurn { delivery_id, .. } =
                host.provider.commands.recv_blocking().unwrap()
            else {
                panic!("send command")
            };
            host.provider
                .events
                .send_blocking(AgentEvent::TurnAccepted { delivery_id })
                .unwrap();
            host.provider
                .events
                .send_blocking(AgentEvent::TurnStarted {
                    turn_id: "first".into(),
                })
                .unwrap();
            host.event(|event| matches!(&event.event, ServerEvent::SessionEvent(record) if matches!(&record.event, AgentEvent::TurnStarted { turn_id } if turn_id == "first")));
        }
    }
    host.query(Principal::Full, Query::Ping);
    host.provider
        .events
        .send_blocking(AgentEvent::TurnCompleted {
            turn_id: "first".into(),
            status: TurnStatus::Completed,
            usage: None,
        })
        .unwrap();
    let SessionCommand::SendTurn { delivery_id, .. } =
        host.provider.commands.recv_blocking().unwrap()
    else {
        panic!("queued send command")
    };
    host.provider
        .events
        .send_blocking(AgentEvent::TurnAccepted { delivery_id })
        .unwrap();
    host.event(|event| matches!(&event.event, ServerEvent::SessionEvent(record) if matches!(&record.event, AgentEvent::ItemCompleted(ThreadItem { content: ItemContent::UserMessage { text, .. }, .. }) if text == "queued")));
    let records = host.finish();
    for (text, device) in [("first", "one"), ("queued", "two")] {
        let record = records.iter().find(|record| matches!(&record.event, AgentEvent::ItemCompleted(ThreadItem { content: ItemContent::UserMessage { text: actual, .. }, .. }) if actual == text)).unwrap();
        assert_eq!(
            record.author,
            Some(Author {
                device_id: device.into(),
                name: format!("Member {device}")
            })
        );
    }
}
