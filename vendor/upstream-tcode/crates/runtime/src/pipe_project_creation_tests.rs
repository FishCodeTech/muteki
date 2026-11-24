use super::*;
use tcode_services::user_files::UserDirectories;

fn fixture(documents: bool, legacy_scratch: bool) -> (SpawnedHost, std::path::PathBuf) {
    let root =
        std::env::temp_dir().join(format!("tcode-project-creation-{}", uuid::Uuid::new_v4()));
    if legacy_scratch {
        let scratch = root.join(if documents {
            "documents/Tcode"
        } else {
            "home/Documents/Tcode"
        });
        std::fs::create_dir_all(root.join("data")).unwrap();
        let project = tcode_core::project::Project::from_root(scratch.clone());
        let mut session = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::Codex,
            scratch.join("2026-01-01"),
            None,
        );
        session.id = "existing-quick-chat".into();
        session.project_id = Some(project.id.clone());
        let mut project = serde_json::to_value(project).unwrap();
        project.as_object_mut().unwrap().remove("quick_chat");
        std::fs::write(
            root.join("data/sessions.json"),
            serde_json::to_vec(&serde_json::json!({
                "projects": [project], "sessions": [session]
            }))
            .unwrap(),
        )
        .unwrap();
    }
    let store = SessionStore::open_at(root.join("data")).unwrap();
    if legacy_scratch {
        store
            .migrate(|_| {}, &std::sync::atomic::AtomicBool::new(false))
            .unwrap();
    }
    let host = spawn_host(
        store,
        HostServices {
            user_directories: UserDirectories {
                home: Some(root.join("home")),
                documents: documents.then(|| root.join("documents")),
            },
            ..HostServices::default()
        },
    )
    .unwrap();
    (host, root)
}

#[test]
fn create_new_project_creates_and_registers_a_host_directory() {
    let (host, root) = fixture(true, false);
    let link = host.link();
    let events = super::p4b_tests::observed_events(&link);
    link.subscribe(Subscription {
        known_index: None,
        topic: Topic::Index,
        after: None,
    })
    .unwrap();
    let command = Command::CreateNewProject {
        name: "  my-project  ".into(),
    };
    let CommandResponse::ProjectId(Some(id)) = link.command_blocking(command.clone()).unwrap()
    else {
        panic!("expected project id");
    };
    let expected = root.join("home/TcodeProjects/my-project");
    assert!(expected.is_dir());
    let event = super::tests::next_event(
        &events,
        |event| matches!(&event.event, ServerEvent::IndexUpsertProject(project) if project.id == id),
    );
    let ServerEvent::IndexUpsertProject(project) = event.event else {
        unreachable!()
    };
    assert_eq!(project.root, expected);
    assert!(!project.quick_chat);
    assert_eq!(
        link.command_blocking(command).unwrap(),
        CommandResponse::ProjectId(Some(id))
    );
    for name in ["nested/project", r"nested\project", "", " ", ".", ".."] {
        let error = link
            .command_blocking(Command::CreateNewProject { name: name.into() })
            .unwrap_err();
        assert_eq!(error.code, "invalid_project_root", "name: {name:?}");
    }
    assert!(!root.join("home/TcodeProjects/nested").exists());
    host.shutdown_blocking().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn scratch_draft_uses_todays_host_documents_directory_and_reuses_the_draft() {
    for (documents, legacy) in [(true, false), (false, false), (true, true), (false, true)] {
        let (host, root) = fixture(documents, legacy);
        let link = host.link();
        let events = super::p4b_tests::observed_events(&link);
        link.subscribe(Subscription {
            known_index: None,
            topic: Topic::Index,
            after: None,
        })
        .unwrap();
        let snapshot = super::tests::next_event(&events, |event| {
            matches!(&event.event, ServerEvent::IndexSnapshot(_))
        });
        let ServerEvent::IndexSnapshot(snapshot) = snapshot.event else {
            unreachable!()
        };
        let legacy_project = if legacy {
            let project = snapshot
                .projects
                .iter()
                .find(|project| {
                    snapshot.sessions.iter().any(|session| {
                        session.id == "existing-quick-chat"
                            && session.project_id.as_deref() == Some(project.id.as_str())
                    })
                })
                .unwrap();
            assert!(
                project.quick_chat,
                "existing scratch threads are replicated as quick chats"
            );
            Some(project.clone())
        } else {
            None
        };
        let started = chrono::Local::now().date_naive();
        let CommandResponse::SessionId(Some(id)) =
            link.command_blocking(Command::StartScratchDraft).unwrap()
        else {
            panic!("expected session id");
        };
        let project_root = root.join(if documents {
            "documents/Tcode"
        } else {
            "home/Documents/Tcode"
        });
        let project = legacy_project.unwrap_or_else(|| {
            let event = super::tests::next_event(&events, |event| {
                matches!(&event.event, ServerEvent::IndexUpsertProject(_))
            });
            let ServerEvent::IndexUpsertProject(project) = event.event else {
                unreachable!()
            };
            project
        });
        assert_eq!(project.root, project_root);
        assert!(
            project.quick_chat,
            "new scratch drafts are replicated as quick chats"
        );
        link.subscribe(Subscription {
            known_index: None,
            topic: Topic::SessionStatus {
                session_id: id.clone(),
            },
            after: None,
        })
        .unwrap();
        let session = super::tests::next_event(&events, |event| {
            event.topic
                == (Topic::SessionStatus {
                    session_id: id.clone(),
                })
                && matches!(&event.event, ServerEvent::SessionStatusReplaced(_))
        });
        let ServerEvent::SessionStatusReplaced(status) = session.event else {
            unreachable!()
        };
        let repeated = link.command_blocking(Command::StartScratchDraft).unwrap();
        // A run that crosses midnight legitimately lands on two dated
        // directories; only a same-day run can assert the exact path and reuse.
        if chrono::Local::now().date_naive() == started {
            assert_eq!(status.cwd, project_root.join(started.to_string()));
            assert_eq!(repeated, CommandResponse::SessionId(Some(id)));
        }
        assert!(status.cwd.is_dir());
        assert!(status.draft);
        assert_eq!(status.project_id.as_deref(), Some(project.id.as_str()));
        host.shutdown_blocking().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }
}
