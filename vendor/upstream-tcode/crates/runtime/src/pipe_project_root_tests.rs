use super::*;
use agent::ProviderKind;
use std::path::PathBuf;
use tcode_core::project::{IndexFile, Project, SessionMeta, WorktreeInfo};
use tcode_services::store::Mutation;

struct Fixture {
    host: SpawnedHost,
    link: HostLink,
    events: async_channel::Receiver<EventEnvelope>,
    temp: PathBuf,
    project: Project,
}

/// A project at `temp/old` with a thread in its root, one in a subdirectory
/// and one in a worktree elsewhere.
fn fixture() -> Fixture {
    let temp = std::env::temp_dir().join(format!("tcode-project-root-{}", uuid::Uuid::new_v4()));
    let project = Project::from_root(temp.join("old"));
    std::fs::create_dir_all(project.root.join("src")).unwrap();
    std::fs::write(project.root.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::create_dir_all(temp.join("worktrees/wt")).unwrap();
    let store = SessionStore::open_at(temp.join("data")).unwrap();
    let mut mutations = vec![Mutation::upsert_project(project.clone())];
    for (id, cwd, worktree) in [
        ("root", project.root.clone(), None),
        ("sub", project.root.join("src"), None),
        (
            "wt",
            temp.join("worktrees/wt"),
            Some(WorktreeInfo {
                root_project_path: project.root.clone(),
                base: "main".into(),
                branch: "tcode/wt".into(),
            }),
        ),
    ] {
        let mut meta = SessionMeta::new(ProviderKind::Codex, cwd, None);
        meta.id = id.into();
        meta.title = id.into();
        meta.project_id = Some(project.id.clone());
        meta.worktree = worktree;
        meta.resume_cursor = Some(agent::ResumeCursor(serde_json::json!({"thread_id": id})));
        mutations.push(Mutation::upsert_meta(meta));
    }
    store.apply(&mutations).unwrap();
    let host = spawn_host(store.clone(), HostServices::default()).unwrap();
    let link = host.link();
    let events = super::p4b_tests::observed_events(&link);
    link.subscribe(Subscription {
        known_index: None,
        topic: Topic::Index,
        after: None,
    })
    .unwrap();
    Fixture {
        host,
        link,
        events,
        temp,
        project,
    }
}

fn set_root(fixture: &Fixture, root: PathBuf, move_files: bool) -> Result<(), ProtocolError> {
    fixture
        .link
        .command_blocking(Command::SetProjectRoot {
            project_id: fixture.project.id.clone(),
            root,
            move_files,
        })
        .map(|response| assert_eq!(response, CommandResponse::Unit))
}

/// Stop the host and read what it persisted.
fn stored(fixture: &Fixture) -> IndexFile {
    fixture.host.shutdown_blocking().unwrap();
    SessionStore::open_at(fixture.temp.join("data"))
        .unwrap()
        .read_file()
        .unwrap()
}

fn cwds(stored: &IndexFile) -> Vec<(String, PathBuf, Option<String>)> {
    let mut metas: Vec<_> = stored
        .sessions
        .iter()
        .cloned()
        .map(|meta| (meta.id, meta.cwd, meta.project_id))
        .collect();
    metas.sort();
    metas
}

#[test]
fn moving_a_project_carries_its_directory_and_threads_to_the_new_root() {
    let fixture = fixture();
    let new_root = fixture.temp.join("moved");
    set_root(&fixture, new_root.clone(), true).unwrap();

    assert!(!fixture.project.root.exists());
    assert_eq!(
        std::fs::read_to_string(new_root.join("src/main.rs")).unwrap(),
        "fn main() {}\n"
    );
    let event = super::tests::next_event(
        &fixture.events,
        |event| matches!(&event.event, ServerEvent::IndexUpsertProject(project) if project.root == new_root),
    );
    let ServerEvent::IndexUpsertProject(project) = event.event else {
        unreachable!()
    };
    assert_eq!(project.id, fixture.project.id);
    assert_eq!(project.name, "moved");
    let stored = stored(&fixture);
    assert_eq!(stored.projects[0].root, new_root);
    let project_id = Some(fixture.project.id.clone());
    assert_eq!(
        cwds(&stored),
        [
            ("root".to_string(), new_root.clone(), project_id.clone()),
            ("sub".to_string(), new_root.join("src"), project_id.clone()),
            (
                "wt".to_string(),
                fixture.temp.join("worktrees/wt"),
                project_id
            ),
        ]
    );
    let worktree = stored
        .sessions
        .iter()
        .find(|meta| meta.id == "wt")
        .and_then(|meta| meta.worktree.clone())
        .unwrap();
    assert_eq!(worktree.root_project_path, new_root);
    let _ = std::fs::remove_dir_all(&fixture.temp);
}

#[test]
fn repointing_after_a_manual_move_keeps_threads_and_refuses_absent_or_relative_roots() {
    let fixture = fixture();
    let new_root = fixture.temp.join("elsewhere");
    std::fs::rename(&fixture.project.root, &new_root).unwrap();

    let missing = set_root(&fixture, fixture.temp.join("nowhere"), false).unwrap_err();
    assert_eq!(missing.code, "invalid_project_root");
    let relative = set_root(&fixture, PathBuf::from("relative/path"), false).unwrap_err();
    assert_eq!(relative.code, "invalid_project_root");
    let unknown = fixture
        .link
        .command_blocking(Command::SetProjectRoot {
            project_id: "missing".into(),
            root: new_root.clone(),
            move_files: false,
        })
        .unwrap_err();
    assert_eq!(unknown.code, "unknown_project");

    set_root(&fixture, new_root.clone(), false).unwrap();
    let stored = stored(&fixture);
    let project_id = Some(fixture.project.id.clone());
    assert_eq!(
        cwds(&stored)[..2],
        [
            ("root".to_string(), new_root.clone(), project_id.clone()),
            ("sub".to_string(), new_root.join("src"), project_id),
        ]
    );
    let _ = std::fs::remove_dir_all(&fixture.temp);
}

#[test]
fn a_project_already_at_the_new_root_absorbs_the_moved_one() {
    let fixture = fixture();
    let other = Project::from_root(fixture.temp.join("other"));
    std::fs::create_dir_all(&other.root).unwrap();
    let CommandResponse::ProjectId(Some(other_id)) = fixture
        .link
        .command_blocking(Command::CreateProject {
            root: other.root.clone(),
        })
        .unwrap()
    else {
        panic!("expected project id");
    };

    let occupied = set_root(&fixture, other.root.clone(), true).unwrap_err();
    assert_eq!(occupied.code, "invalid_project_root");
    assert!(
        fixture.project.root.is_dir(),
        "a refused move touches nothing"
    );

    set_root(&fixture, other.root.clone(), false).unwrap();
    super::tests::next_event(
        &fixture.events,
        |event| matches!(&event.event, ServerEvent::IndexRemoveProject { project_id } if *project_id == fixture.project.id),
    );
    let stored = stored(&fixture);
    assert_eq!(
        stored
            .projects
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>(),
        [other_id.as_str()]
    );
    let other_id = Some(other_id);
    assert_eq!(
        cwds(&stored),
        [
            ("root".to_string(), other.root.clone(), other_id.clone()),
            ("sub".to_string(), other.root.join("src"), other_id.clone()),
            (
                "wt".to_string(),
                fixture.temp.join("worktrees/wt"),
                other_id
            ),
        ]
    );
    let _ = std::fs::remove_dir_all(&fixture.temp);
}
