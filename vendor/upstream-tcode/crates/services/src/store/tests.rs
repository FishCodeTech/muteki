use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

use agent::{ProviderCommand, ProviderCommandKind, TurnStatus};
use tcode_core::project::SettledOverride;

use std::sync::atomic::AtomicBool;

use super::*;

/// A data dir that is removed when the test ends.
struct DataDir(PathBuf);

impl DataDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("tcode-store-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn store(&self) -> SessionStore {
        SessionStore::open_at(self.0.clone()).unwrap()
    }

    /// A store over this data dir after the migration a start runs first.
    fn migrated(&self) -> SessionStore {
        let store = self.store();
        assert_eq!(migrate(&store).unwrap(), Migration::Completed);
        store
    }
}

fn migrate(store: &SessionStore) -> io::Result<Migration> {
    store.migrate(|_| {}, &AtomicBool::new(false))
}

impl Drop for DataDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A log as older builds wrote it, with every irregularity a real one can
/// hold: a legacy bare event, CRLF, a blank line, an unparseable line, a line
/// that is not UTF-8, and a torn last line without its newline.
const MIXED_LOG: &[u8] = b"{\"type\":\"turn_started\",\"turn_id\":\"legacy\"}\n\
{\"ts\":2000,\"event\":{\"type\":\"turn_completed\",\"turn_id\":\"legacy\",\"status\":\"completed\",\"usage\":null}}\r\n\
\n\
{not valid json}\n\
\xff\xfe{\"type\":\"turn_started\",\"turn_id\":\"not-utf8\"}\n\
{\"ts\":2500,\"event\":{\"type\":\"turn_started\",\"turn_id\":\"second\"}}\n\
{\"type\":\"turn_started";

const ORPHAN_LOG: &[u8] =
    b"{\"ts\":1,\"event\":{\"type\":\"turn_started\",\"turn_id\":\"orphan\"}}\n";

/// `sessions.json` in the current object schema, with one session that
/// carries fields older builds wrote and this one dropped, and one that has
/// no project yet and was settled before settling recorded an override.
fn legacy_index() -> serde_json::Value {
    serde_json::json!({
        "projects": [
            {"id": "p1", "name": "alpha", "root": "/work/alpha", "created_at": 1}
        ],
        "sessions": [
            {
                "id": "mixed", "title": "Mixed", "provider": "codex", "cwd": "/work/alpha",
                "project_id": "p1", "created_at": 1, "updated_at": 30,
                "forked_from": "source",
                "checkpoints": [{"turn": 2, "commit": "deadbeef", "event_offset": 7}]
            },
            {
                "id": "empty", "title": "Empty", "provider": "claude_code",
                "cwd": "/work/beta", "created_at": 2, "updated_at": 20, "settled_at": 15
            }
        ]
    })
}

fn write_legacy_fixture(root: &Path) {
    fs::write(root.join("sessions.json"), legacy_index().to_string()).unwrap();
    fs::write(root.join("mixed.jsonl"), MIXED_LOG).unwrap();
    fs::write(root.join("empty.jsonl"), b"").unwrap();
    fs::write(root.join("orphan.jsonl"), ORPHAN_LOG).unwrap();
}

const LEGACY_FILES: [&str; 4] = [
    "sessions.json",
    "mixed.jsonl",
    "empty.jsonl",
    "orphan.jsonl",
];

fn turn_ids(events: &[StoredEvent]) -> Vec<(Option<u64>, String)> {
    events
        .iter()
        .map(|stored| {
            let id = match &stored.event {
                AgentEvent::TurnStarted { turn_id } => format!("started {turn_id}"),
                AgentEvent::TurnCompleted {
                    turn_id,
                    status: TurnStatus::Completed,
                    ..
                } => format!("completed {turn_id}"),
                other => format!("{other:?}"),
            };
            (stored.ts, id)
        })
        .collect()
}

/// What the fixture must read back as, wherever it was migrated.
fn assert_fixture_migrated(store: &SessionStore, root: &Path) {
    let file = store.read_file().unwrap();
    assert_eq!(file.projects.len(), 2);
    assert_eq!(file.projects[0].id, "p1");
    let derived = &file.projects[1];
    assert_eq!(derived.root, Path::new("/work/beta"));
    assert_eq!(derived.name, "beta");
    assert_eq!(
        file.sessions
            .iter()
            .map(|meta| (meta.id.as_str(), meta.project_id.as_deref()))
            .collect::<Vec<_>>(),
        [("mixed", Some("p1")), ("empty", Some(derived.id.as_str()))]
    );
    let mixed = &file.sessions[0];
    assert_eq!(mixed.title, "Mixed");
    assert_eq!(mixed.archived_at, None);
    assert_eq!(mixed.settled_override, None);
    let empty = &file.sessions[1];
    assert_eq!(empty.settled_at, Some(15));
    assert_eq!(empty.settled_override, Some(SettledOverride::Settled));

    assert_eq!(store.read_event_log("mixed").unwrap(), MIXED_LOG);
    assert_eq!(store.read_event_log("orphan").unwrap(), ORPHAN_LOG);
    assert!(store.read_event_log("empty").unwrap().is_empty());
    assert_eq!(
        turn_ids(&store.read_events("mixed").unwrap()),
        [
            (None, "started legacy".to_owned()),
            (Some(2000), "completed legacy".to_owned()),
            (Some(2500), "started second".to_owned()),
        ]
    );
    assert_eq!(
        turn_ids(&store.read_events("orphan").unwrap()),
        [(Some(1), "started orphan".to_owned())]
    );

    for name in LEGACY_FILES {
        assert!(!root.join(name).exists(), "{name} left in the data dir");
    }
    for leftover in [
        LEGACY_DIR_NAME,
        "tcode.db.migrating",
        "tcode.db.migrating-wal",
        "tcode.db.archive",
    ] {
        assert!(!root.join(leftover).exists(), "{leftover} left behind");
    }
}

#[test]
fn pinned_history_cuts_survive_mutation_and_close_releases_readers() {
    use std::borrow::Cow;
    let dir = DataDir::new();
    let store = dir.store();
    let old = AgentEvent::Warning {
        message: "old".repeat(1000),
    };
    store
        .apply(&[Mutation::append_event("thread", 1, &old).unwrap()])
        .unwrap();
    let cut = store.snapshot_log("thread").unwrap();
    store
        .append_event(
            "thread",
            2,
            &AgentEvent::Warning {
                message: "appended".into(),
            },
        )
        .unwrap();
    assert_eq!(cut.end, 1);
    assert_eq!(
        cut.index,
        Some(TurnIndex {
            turns: 0,
            starts: vec![]
        })
    );
    let mut seen = Vec::new();
    cut.visit_projected_rows(
        0..u64::MAX,
        false,
        "fixture-preview",
        128,
        |record| {
            Cow::Owned(StoredEvent {
                event: AgentEvent::Warning {
                    message: "old-preview".into(),
                },
                ..record.clone()
            })
        },
        |row, record| {
            seen.push((row, record.event));
            true
        },
    )
    .unwrap();
    assert_eq!(
        seen,
        [(
            0,
            AgentEvent::Warning {
                message: "old-preview".into()
            }
        )]
    );
    let publication = cut.take_publication();
    store
        .apply(&[Mutation::replace_event_log(
            "thread",
            b"{\"type\":\"warning\",\"message\":\"replacement\"}\n".to_vec(),
        )])
        .unwrap();
    publication.publish().unwrap();
    let current = store.snapshot_log("thread").unwrap();
    let mut seen = Vec::new();
    current
        .visit_projected_rows(
            0..current.end,
            false,
            "fixture-preview",
            128,
            |record| Cow::Borrowed(record),
            |_, record| {
                seen.push(record.event);
                true
            },
        )
        .unwrap();
    assert_eq!(
        seen,
        [AgentEvent::Warning {
            message: "replacement".into()
        }]
    );
    store.close().unwrap();
    assert!(
        current
            .visit_projected_rows(
                0..current.end,
                false,
                "fixture-preview",
                128,
                |record| Cow::Borrowed(record),
                |_, _| true
            )
            .is_err()
    );
}

#[test]
fn first_start_migrates_the_json_index_and_every_log_byte_for_byte() {
    let dir = DataDir::new();
    write_legacy_fixture(dir.path());
    let store = dir.store();
    assert!(store.needs_migration().unwrap());
    let error = store.open().unwrap_err();
    assert!(error.to_string().contains("not been migrated"), "{error}");
    assert_eq!(migrate(&store).unwrap(), Migration::Completed);
    assert!(!store.needs_migration().unwrap());
    store.open().unwrap();
    assert_fixture_migrated(&store, dir.path());

    // Appending to a log that lost its final newline terminates that line
    // first, as appending to the file did.
    store
        .append_event(
            "mixed",
            3000,
            &AgentEvent::TurnStarted {
                turn_id: "next".into(),
            },
        )
        .unwrap();
    let mut expected = MIXED_LOG.to_vec();
    expected.extend_from_slice(
        b"\n{\"ts\":3000,\"event\":{\"type\":\"turn_started\",\"turn_id\":\"next\"}}\n",
    );
    assert_eq!(store.read_event_log("mixed").unwrap(), expected);
    store.close().unwrap();
    assert!(store.read_file().is_err(), "a closed store serves nothing");

    // A later start opens the database as it is: the data dir now holds only
    // what an older build would write, and that is ignored.
    fs::write(dir.path().join("sessions.json"), b"[]").unwrap();
    fs::write(dir.path().join("mixed.jsonl"), b"").unwrap();
    let reopened = dir.store();
    assert!(!reopened.needs_migration().unwrap());
    assert_eq!(reopened.read_event_log("mixed").unwrap(), expected);
    assert_eq!(reopened.read_file().unwrap().sessions.len(), 2);
    assert_eq!(fs::read(dir.path().join("sessions.json")).unwrap(), b"[]");
}

#[test]
fn legacy_bare_array_index_migrates_with_one_derived_project_per_root() {
    let dir = DataDir::new();
    // Old-format file: a bare JSON array with no project_id fields.
    let legacy = serde_json::json!([
        {
            "id": "s1", "title": "One", "provider": "claude_code",
            "cwd": "/work/alpha", "created_at": 1, "updated_at": 10
        },
        {
            "id": "s2", "title": "Two", "provider": "codex",
            "cwd": "/work/alpha", "created_at": 2, "updated_at": 20
        },
        {
            "id": "s3", "title": "Three", "provider": "codex",
            "cwd": "/work/beta", "created_at": 3, "updated_at": 30
        }
    ]);
    fs::write(dir.path().join("sessions.json"), legacy.to_string()).unwrap();

    let file = dir.migrated().read_file().unwrap();
    // Two distinct roots -> two derived projects, deduped by root.
    assert_eq!(file.projects.len(), 2);
    let alpha = file
        .projects
        .iter()
        .find(|p| p.root == Path::new("/work/alpha"))
        .unwrap();
    assert_eq!(alpha.name, "alpha");
    let project_of = |id: &str| {
        file.sessions
            .iter()
            .find(|session| session.id == id)
            .unwrap()
            .project_id
            .clone()
    };
    assert_eq!(project_of("s1"), Some(alpha.id.clone()));
    assert_eq!(project_of("s2"), project_of("s1"));
    assert_ne!(project_of("s3"), project_of("s1"));
    // Newest first.
    assert_eq!(
        dir.store()
            .load_index()
            .unwrap()
            .iter()
            .map(|meta| meta.id.as_str())
            .collect::<Vec<_>>(),
        ["s3", "s2", "s1"]
    );
}

#[test]
fn an_unparseable_index_does_not_stop_the_logs_migrating() {
    let dir = DataDir::new();
    let corrupt = b"not valid session json";
    fs::write(dir.path().join("sessions.json"), corrupt).unwrap();
    fs::write(dir.path().join("orphan.jsonl"), ORPHAN_LOG).unwrap();
    let store = dir.migrated();

    let file = store.read_file().unwrap();
    assert!(file.projects.is_empty() && file.sessions.is_empty());
    assert_eq!(store.read_event_log("orphan").unwrap(), ORPHAN_LOG);
    assert!(!dir.path().join("sessions.json").exists());
}

#[test]
fn duplicate_index_ids_fail_the_migration_and_leave_the_sources_alone() {
    let dir = DataDir::new();
    let session = serde_json::json!({
        "id": "twice", "title": "One", "provider": "codex",
        "cwd": "/work", "created_at": 1, "updated_at": 1
    });
    let index = serde_json::json!({"projects": [], "sessions": [session, session]}).to_string();
    fs::write(dir.path().join("sessions.json"), &index).unwrap();
    fs::write(dir.path().join("twice.jsonl"), ORPHAN_LOG).unwrap();

    let error = migrate(&dir.store()).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("twice"), "{error}");
    assert!(!dir.path().join(DB_FILE).exists());
    assert!(!dir.path().join(LEGACY_DIR_NAME).exists());
    assert_eq!(
        fs::read(dir.path().join("sessions.json")).unwrap(),
        index.as_bytes()
    );
    assert_eq!(
        fs::read(dir.path().join("twice.jsonl")).unwrap(),
        ORPHAN_LOG
    );
}

const LEGACY_DIR_NAME: &str = "legacy";

fn archive_list(names: &[&str]) -> Vec<u8> {
    serde_json::to_vec(names).unwrap()
}

#[test]
fn a_start_after_an_interrupted_migration_discards_the_staging_files_and_starts_over() {
    // Killed while building: a torn staging file and WAL, no final database.
    let dir = DataDir::new();
    write_legacy_fixture(dir.path());
    fs::write(dir.path().join("tcode.db.migrating"), b"partial").unwrap();
    fs::write(dir.path().join("tcode.db.migrating-wal"), b"torn frames").unwrap();
    let store = dir.migrated();
    assert_fixture_migrated(&store, dir.path());
    store.close().unwrap();

    // Killed after the staged database was complete and the archive list was
    // written, but before the rename was durable: the sources are all still
    // in the data dir.
    fs::rename(
        dir.path().join(DB_FILE),
        dir.path().join("tcode.db.migrating"),
    )
    .unwrap();
    write_legacy_fixture(dir.path());
    fs::write(
        dir.path().join("tcode.db.archive"),
        archive_list(&LEGACY_FILES),
    )
    .unwrap();
    let store = dir.migrated();
    assert_fixture_migrated(&store, dir.path());
}

/// Every file in `root`, by name, with its bytes; `legacy/` as a marker.
/// The store's own files are recorded by name only: Windows locks the whole
/// of a file the store holds, so even the empty `tcode.lock` cannot be read.
fn snapshot(root: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    fs::read_dir(root)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            let bytes = if entry.file_type().unwrap().is_dir() {
                b"<dir>".to_vec()
            } else if name == LOCK_FILE || name.starts_with(DB_FILE) {
                b"<store file>".to_vec()
            } else {
                fs::read(entry.path()).unwrap()
            };
            (name, bytes)
        })
        .collect()
}

#[test]
fn a_cancelled_migration_changes_no_source_and_the_next_one_completes() {
    let dir = DataDir::new();
    write_legacy_fixture(dir.path());
    // Longer than one chunk, so a cancel can land inside a log.
    let bulk_line = format!(
        "{{\"ts\":1,\"event\":{{\"type\":\"turn_started\",\"turn_id\":\"{}\"}}}}\n",
        "x".repeat(4000)
    );
    let bulk = bulk_line.repeat(2600);
    assert!(bulk.len() > migrate::CHUNK_BYTES);
    fs::write(dir.path().join("bulk.jsonl"), &bulk).unwrap();
    let store = dir.store();
    assert!(store.needs_migration().unwrap());
    let mut sources = snapshot(dir.path());
    // Owned for the whole start, and kept from the first check.
    sources.remove(LOCK_FILE);

    type Stop = fn(&MigrationProgress) -> bool;
    let stops: [(&str, Stop); 4] = [
        ("scanning", |progress| {
            progress.phase == MigrationPhase::Scanning
        }),
        ("inside a log", |progress| {
            progress.phase == MigrationPhase::Importing
                && progress.bytes_done > 0
                && progress.threads_done == 0
        }),
        ("between logs", |progress| {
            progress.phase == MigrationPhase::Importing && progress.threads_done == 2
        }),
        ("verifying", |progress| {
            progress.phase == MigrationPhase::Verifying && progress.threads_done == 1
        }),
    ];
    for (point, stop) in stops {
        let cancel = AtomicBool::new(false);
        let mut seen = None;
        let outcome = store
            .migrate(
                |progress| {
                    if !cancel.load(Ordering::Relaxed) && stop(&progress) {
                        seen = Some(progress);
                        cancel.store(true, Ordering::Relaxed);
                    }
                },
                &cancel,
            )
            .unwrap();
        assert_eq!(outcome, Migration::Cancelled, "{point}");
        assert!(seen.is_some(), "{point}: never reached");
        let mut after = snapshot(dir.path());
        assert!(after.remove(LOCK_FILE).is_some());
        assert_eq!(after, sources, "{point}");
        assert!(store.needs_migration().unwrap(), "{point}");
    }

    let mut reports = Vec::new();
    let outcome = store
        .migrate(|progress| reports.push(progress), &AtomicBool::new(false))
        .unwrap();
    assert_eq!(outcome, Migration::Completed);
    let mut phases = reports
        .iter()
        .map(|progress| progress.phase)
        .collect::<Vec<_>>();
    phases.dedup();
    assert_eq!(
        phases,
        [
            MigrationPhase::Scanning,
            MigrationPhase::Importing,
            MigrationPhase::Verifying,
            MigrationPhase::Publishing,
            MigrationPhase::Archiving
        ]
    );
    let log_bytes = (MIXED_LOG.len() + ORPHAN_LOG.len() + bulk.len()) as u64;
    for phase in [MigrationPhase::Importing, MigrationPhase::Verifying] {
        let last = reports
            .iter()
            .rfind(|progress| progress.phase == phase)
            .unwrap();
        assert_eq!(
            (
                last.threads_done,
                last.threads_total,
                last.bytes_done,
                last.bytes_total
            ),
            (4, 4, log_bytes, log_bytes),
            "{phase:?}"
        );
    }
    let last = reports.last().unwrap();
    assert_eq!(
        (last.threads_done, last.bytes_done),
        (last.threads_total, last.bytes_total)
    );
    assert_eq!(store.read_event_log("bulk").unwrap(), bulk.as_bytes());
    assert_fixture_migrated(&store, dir.path());
}

#[test]
fn a_start_after_an_interrupted_archival_finishes_it_and_loses_no_source() {
    let dir = DataDir::new();
    write_legacy_fixture(dir.path());
    dir.migrated().close().unwrap();

    // Killed after the rename while moving the sources: two are still in the
    // data dir. One of them also has a copy in legacy/ already, so the source
    // stays where it is rather than replace it.
    fs::rename(
        dir.path().join("legacy/mixed.jsonl"),
        dir.path().join("mixed.jsonl"),
    )
    .unwrap();
    fs::copy(
        dir.path().join("legacy/orphan.jsonl"),
        dir.path().join("orphan.jsonl"),
    )
    .unwrap();
    fs::write(dir.path().join("legacy/orphan.jsonl"), b"older copy").unwrap();
    fs::write(
        dir.path().join("tcode.db.archive"),
        archive_list(&LEGACY_FILES),
    )
    .unwrap();

    let store = dir.store();
    store.open().unwrap();
    assert!(!dir.path().join("tcode.db.archive").exists());
    assert!(!dir.path().join("mixed.jsonl").exists());
    assert_eq!(
        fs::read(dir.path().join("orphan.jsonl")).unwrap(),
        ORPHAN_LOG
    );
    // Removed with the archived sources once the database opened.
    assert!(!dir.path().join(LEGACY_DIR_NAME).exists());
    assert_eq!(store.read_event_log("mixed").unwrap(), MIXED_LOG);
}

#[test]
fn a_fresh_data_dir_gets_a_completed_database_through_the_staged_path() {
    let dir = DataDir::new();
    let store = dir.store();
    assert!(store.load_index().unwrap().is_empty());
    store.close().unwrap();
    let db = db::Db::open(&dir.path().join(DB_FILE), false).unwrap();
    assert_eq!(db.user_version().unwrap(), SCHEMA_VERSION);
    drop(db);
    assert!(!dir.path().join("legacy").exists());
    assert!(!dir.path().join("tcode.db.migrating").exists());
}

#[test]
fn an_unfinished_newer_or_corrupt_database_is_refused_and_left_untouched() {
    let set_version = |root: &Path, version: i64| {
        let db = db::Db::open(&root.join(DB_FILE), false).unwrap();
        db.write(|db, connection| {
            db.execute(connection, &format!("PRAGMA user_version = {version}"), ())
                .map(drop)
        })
        .unwrap();
        db.checkpoint().unwrap();
    };
    for (version, expected) in [(0, "never completed"), (2, "newer Tcode")] {
        let dir = DataDir::new();
        dir.store()
            .upsert_meta(&SessionMeta::new(ProviderKind::Codex, "/w".into(), None))
            .unwrap();
        dir.store().close().unwrap();
        set_version(dir.path(), version);
        // The sources a migration archived stay until a database opens.
        fs::create_dir(dir.path().join(LEGACY_DIR_NAME)).unwrap();
        fs::write(dir.path().join("legacy/mixed.jsonl"), MIXED_LOG).unwrap();
        let before = fs::read(dir.path().join(DB_FILE)).unwrap();
        let error = dir.store().open().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains(expected), "{error}");
        assert_eq!(fs::read(dir.path().join(DB_FILE)).unwrap(), before);
        assert_eq!(
            fs::read(dir.path().join("legacy/mixed.jsonl")).unwrap(),
            MIXED_LOG
        );
    }

    let dir = DataDir::new();
    let garbage = vec![0x5a_u8; 8192];
    fs::write(dir.path().join(DB_FILE), &garbage).unwrap();
    let error = dir.store().open().unwrap_err();
    assert!(error.to_string().contains("sqlite3"), "{error}");
    assert!(error.to_string().contains(DB_FILE), "{error}");
    assert_eq!(fs::read(dir.path().join(DB_FILE)).unwrap(), garbage);
}

#[test]
fn session_index_upserts_orders_and_removes_only_the_selected_session() {
    let dir = DataDir::new();
    let store = dir.store();
    let mut a = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/a"), None);
    a.updated_at = 100;
    let mut b = SessionMeta::new(ProviderKind::ClaudeCode, PathBuf::from("/b"), None);
    b.updated_at = 200;
    store.upsert_meta(&a).unwrap();
    store.upsert_meta(&b).unwrap();

    let index = store.load_index().unwrap();
    assert_eq!(index.len(), 2);
    // newest first
    assert_eq!(index[0].id, b.id);
    assert_eq!(index[1].id, a.id);

    // upsert replaces
    let mut a2 = a.clone();
    a2.title = "renamed".into();
    store.upsert_meta(&a2).unwrap();
    let index = store.load_index().unwrap();
    assert_eq!(index.len(), 2);
    assert_eq!(
        index.iter().find(|m| m.id == a.id).unwrap().title,
        "renamed"
    );
    for (meta, ts) in [(&a, 1), (&b, 2)] {
        store
            .append_event(
                &meta.id,
                ts,
                &AgentEvent::TurnStarted {
                    turn_id: format!("turn-{ts}"),
                },
            )
            .unwrap();
    }
    store.remove_session(&a.id).unwrap();
    store.remove_session(&a.id).unwrap();
    let remaining = store.load_index().unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, b.id);
    assert!(store.read_event_log(&a.id).unwrap().is_empty());
    assert_eq!(
        store.read_event_log(&b.id).unwrap(),
        b"{\"ts\":2,\"event\":{\"type\":\"turn_started\",\"turn_id\":\"turn-2\"}}\n"
    );
}

#[test]
fn a_cloned_or_imported_log_keeps_its_exact_bytes() {
    let dir = DataDir::new();
    let store = dir.store();
    let authored = b"{\"ts\":3000,\"author\":{\"device_id\":\"phone\",\"name\":\"Alice\"},\"event\":{\"type\":\"turn_started\",\"turn_id\":\"shared\"}}\n";
    let mut log = MIXED_LOG.to_vec();
    log.push(b'\n');
    log.extend_from_slice(authored);
    let fork = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/w"), None);
    store
        .apply(&[
            Mutation::replace_event_log("source", log.clone()),
            Mutation::clone_events("source", &fork.id),
            Mutation::upsert_meta(fork.clone()),
            Mutation::clone_events("missing", "empty-fork"),
        ])
        .unwrap();
    assert_eq!(store.read_event_log("source").unwrap(), log);
    assert_eq!(store.read_event_log(&fork.id).unwrap(), log);
    let records = store.read_events(&fork.id).unwrap();
    assert_eq!(
        records.last().unwrap().author,
        Some(Author {
            device_id: "phone".into(),
            name: "Alice".into()
        })
    );
    assert!(
        records[..records.len() - 1]
            .iter()
            .all(|record| record.author.is_none())
    );
    assert!(store.read_event_log("empty-fork").unwrap().is_empty());
    assert_eq!(store.load_index().unwrap(), [fork]);
}

#[test]
fn removing_project_cleans_only_managed_icons() {
    let dir = DataDir::new();
    let root = dir.path();
    let store = dir.store();
    fs::create_dir(root.join("project-icons")).unwrap();
    let mut project = Project::from_root(root.join("project"));
    for (path, managed) in [
        (root.join("project-icons/icon.png"), true),
        (root.join("original.png"), false),
    ] {
        fs::write(&path, b"stored image").unwrap();
        project.icon_path = Some(path.clone());
        store.upsert_project(&project).unwrap();
        assert_eq!(
            store.read_file().unwrap().projects[0].icon_path.as_ref(),
            Some(&path)
        );
        store.remove_project(&project.id).unwrap();
        assert!(store.read_file().unwrap().projects.is_empty());
        assert_eq!(path.exists(), !managed);
    }
}

#[test]
fn command_cache_is_kept_per_native_home_and_acp_agent() {
    let dir = DataDir::new();
    let root = dir.path().to_path_buf();
    let store = dir.store();
    let native = vec![ProviderCommand {
        name: "review".into(),
        description: Some("Review the current changes".into()),
        kind: ProviderCommandKind::Command,
    }];
    let acp = vec![ProviderCommand {
        name: "browser".into(),
        description: None,
        kind: ProviderCommandKind::Skill,
    }];
    let default_home = CommandsCacheKey::Native {
        provider: ProviderKind::ClaudeCode,
        home: None,
    };
    let shadow_home = CommandsCacheKey::Native {
        provider: ProviderKind::ClaudeCode,
        home: Some(PathBuf::from("/tmp/claude-shadow")),
    };
    let agent = CommandsCacheKey::Acp {
        agent_id: "vendor/agent".into(),
    };
    store.save_commands(&default_home, &native).unwrap();
    store.save_commands(&agent, &acp).unwrap();

    // Reopen the store to prove the values come from disk, not memory.
    let reopened = SessionStore::open_at(root.clone()).unwrap();
    assert_eq!(reopened.load_commands(&default_home), native);
    assert!(reopened.load_commands(&shadow_home).is_empty());
    assert_eq!(reopened.load_commands(&agent), acp);
    assert!(
        reopened
            .load_commands(&CommandsCacheKey::Acp {
                agent_id: "different-agent".into()
            })
            .is_empty()
    );
    assert!(
        root.join("commands-acp-76656e646f722f6167656e74.json")
            .is_file()
    );

    reopened.save_commands(&shadow_home, &native).unwrap();
    reopened.invalidate_commands(&default_home).unwrap();
    reopened.invalidate_commands(&default_home).unwrap();
    assert!(reopened.load_commands(&default_home).is_empty());
    assert_eq!(reopened.load_commands(&shadow_home), native);
}

/// This test binary, re-run as another process that starts as a host does —
/// with `TCODE_DATA_DIR` set to `root`, and `LEGACY_TCODE_DATA_DIR` to
/// `previous` if given — and holds the data dir until its stdin closes. It
/// reports the phases its preparation went through, then `OPEN` or
/// `ERR <kind> <message>`.
struct Child {
    process: std::process::Child,
    stdout: BufReader<std::process::ChildStdout>,
    /// Each phase by its `Debug` name.
    phases: Vec<String>,
    report: String,
}

impl Child {
    fn spawn(root: &Path, previous: Option<&Path>) -> std::process::Child {
        let mut command = crate::process::command(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "store::tests::store_owner_process",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("TCODE_DATA_DIR", root)
            .env_remove("LEGACY_TCODE_DATA_DIR");
        if let Some(previous) = previous {
            command.env("LEGACY_TCODE_DATA_DIR", previous);
        }
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// Wait for the child to say whether it owns the data dir.
    fn report(mut process: std::process::Child) -> Self {
        let mut stdout = BufReader::new(process.stdout.take().unwrap());
        // libtest prints the test's name on the same line first.
        let mut line = String::new();
        let mut phases = Vec::new();
        let report = loop {
            line.clear();
            assert!(
                stdout.read_line(&mut line).unwrap() > 0,
                "the child exited without reporting"
            );
            if let Some((_, phase)) = line.trim_end().split_once(OWNER_PHASE) {
                phases.push(phase.to_owned());
            }
            if let Some((_, report)) = line.trim_end().split_once(OWNER_REPORT) {
                break report.to_owned();
            }
        };
        Self {
            process,
            stdout,
            phases,
            report,
        }
    }

    /// Close the child's stdin, which makes it close the store and exit.
    fn release(mut self) {
        drop(self.process.stdin.take());
        std::io::copy(&mut self.stdout, &mut std::io::sink()).unwrap();
        assert!(self.process.wait().unwrap().success());
    }
}

const OWNER_REPORT: &str = "store owner: ";
const OWNER_PHASE: &str = "store owner phase: ";

#[test]
#[ignore = "the other process of the ownership tests below, which run it"]
fn store_owner_process() {
    let store = SessionStore::open_host(None).unwrap();
    let opened = store.needs_migration().and_then(|needed| {
        if needed {
            let mut last = None;
            let outcome = store.migrate(
                |progress| {
                    if last != Some(progress.phase) {
                        println!("{OWNER_PHASE}{:?}", progress.phase);
                        last = Some(progress.phase);
                    }
                },
                &AtomicBool::new(false),
            )?;
            assert_eq!(outcome, Migration::Completed);
        }
        store.open()
    });
    match opened {
        Ok(()) => println!("{OWNER_REPORT}OPEN"),
        Err(error) => println!("{OWNER_REPORT}ERR {:?} {error}", error.kind()),
    }
    std::io::stdout().flush().unwrap();
    let _ = std::io::stdin().read(&mut [0_u8; 1]);
    store.close().unwrap();
}

#[test]
fn a_second_process_is_refused_and_a_relaunch_waits_for_the_owner_to_exit() {
    let dir = DataDir::new();
    let owner = Child::report(Child::spawn(dir.path(), None));
    assert_eq!(owner.report, "OPEN");

    let error = dir.store().open().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::ResourceBusy);
    assert!(
        error
            .to_string()
            .contains(&dir.path().display().to_string()),
        "{error}"
    );

    // A permission relaunch starts while the old instance is still closing.
    crate::relaunch::write(
        dir.path(),
        &crate::relaunch::RelaunchMarker {
            reopen_settings: "computer_use".into(),
            active_session: None,
        },
    )
    .unwrap();
    let waiting = std::thread::spawn({
        let store = dir.store();
        move || store.open()
    });
    std::thread::sleep(Duration::from_millis(300));
    assert!(!waiting.is_finished(), "the relaunch waits for the owner");
    owner.release();
    waiting.join().unwrap().unwrap();
    // Only peeked: the host consumes the marker once it owns the data dir.
    assert!(crate::relaunch::pending(dir.path()));
}

#[test]
fn two_simultaneous_first_launches_migrate_once() {
    let dir = DataDir::new();
    write_legacy_fixture(dir.path());
    let (first, second) = (
        Child::spawn(dir.path(), None),
        Child::spawn(dir.path(), None),
    );
    let (first, second) = (Child::report(first), Child::report(second));
    let mut reports = [first.report.clone(), second.report.clone()];
    reports.sort();
    assert_eq!(reports[1], "OPEN", "{reports:?}");
    assert!(reports[0].starts_with("ERR ResourceBusy"), "{reports:?}");
    first.release();
    second.release();

    let store = dir.store();
    assert_fixture_migrated(&store, dir.path());
}

/// Every file, directory and link under `root` by relative path: a file's
/// bytes, a link's target, or `<dir>`.
pub(super) fn tree(root: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let kind = fs::symlink_metadata(&path).unwrap().file_type();
            let relative = path.strip_prefix(root).unwrap().to_owned();
            if kind.is_symlink() {
                let target = fs::read_link(&path).unwrap();
                out.insert(relative, target.to_string_lossy().as_bytes().to_vec());
            } else if kind.is_dir() {
                out.insert(relative, b"<dir>".to_vec());
                walk(root, &path, out);
            } else {
                out.insert(relative, fs::read(&path).unwrap());
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// An older build's data dir with every kind of entry it can hold.
pub(super) fn write_older_data_dir(previous: &Path) {
    fs::create_dir_all(previous.join("attachments/session-1")).unwrap();
    fs::create_dir_all(previous.join("project-icons")).unwrap();
    fs::create_dir_all(previous.join("profile-homes/claude/.claude")).unwrap();
    fs::create_dir_all(previous.join("legacy")).unwrap();
    fs::write(previous.join("settings.json"), br#"{"locale":"en"}"#).unwrap();
    fs::write(previous.join("traverse.json"), br#"{"key":"machine"}"#).unwrap();
    fs::write(
        previous.join("attachments/session-1/a.png"),
        [0x89, b'P', b'N', b'G'],
    )
    .unwrap();
    fs::write(previous.join("project-icons/p1.png"), b"icon").unwrap();
    fs::write(
        previous.join("profile-homes/claude/.claude/settings.json"),
        b"{}",
    )
    .unwrap();
    fs::write(previous.join("legacy/older.jsonl"), ORPHAN_LOG).unwrap();
    fs::write(previous.join(".DS_Store"), b"finder").unwrap();
}

#[test]
fn a_start_moves_an_older_data_dir_in_then_migrates_it_and_leaves_nothing_behind() {
    let home = DataDir::new();
    let previous = home.path().join("Application Support/tcode");
    let root = home.path().join(".tcode");
    write_older_data_dir(&previous);
    write_legacy_fixture(&previous);
    // Orchestrate's worktrees already live under ~/.tcode.
    fs::create_dir_all(root.join("worktrees/thread-1")).unwrap();
    fs::write(root.join("worktrees/thread-1/README"), b"checkout").unwrap();
    let mut expected = tree(&previous);
    expected.retain(|path, _| {
        !path.starts_with("legacy")
            && path != Path::new(".DS_Store")
            && !LEGACY_FILES.iter().any(|name| path == Path::new(name))
    });
    expected.extend(tree(&root));

    let owner = Child::report(Child::spawn(&root, Some(&previous)));
    assert_eq!(owner.report, "OPEN");
    assert_eq!(
        owner.phases,
        [
            MigrationPhase::Relocating,
            MigrationPhase::Scanning,
            MigrationPhase::Importing,
            MigrationPhase::Verifying,
            MigrationPhase::Publishing,
            MigrationPhase::Archiving,
        ]
        .map(|phase| format!("{phase:?}"))
    );
    owner.release();

    assert!(!previous.exists(), "the older data dir is left behind");
    let mut after = tree(&root);
    // The store's own files, `tcode.db` among them.
    after.retain(|path, _| {
        path != Path::new(LOCK_FILE) && !path.to_string_lossy().starts_with(DB_FILE)
    });
    assert_eq!(after, expected);
    assert_fixture_migrated(&SessionStore::open_at(root.clone()).unwrap(), &root);
}

/// Kill the migration at random points — while logs are copied, around the
/// checkpoint, the rename, the directory syncs and the archival — and restart
/// it: there is either no `tcode.db` and every source is where it was, or a
/// complete one, and the finished migration always holds every source byte
/// for byte. `TCODE_MIGRATION_KILLS` sets the number of kills.
#[test]
#[ignore = "spawns and SIGKILLs child processes over a large fixture; run deliberately"]
fn migration_survives_sigkill() {
    let runs: usize = std::env::var("TCODE_MIGRATION_KILLS")
        .ok()
        .and_then(|runs| runs.parse().ok())
        .unwrap_or(20);
    let originals = DataDir::new();
    write_legacy_fixture(originals.path());
    let line = format!(
        "{{\"ts\":1,\"event\":{{\"type\":\"turn_started\",\"turn_id\":\"{}\"}}}}\n",
        "x".repeat(1000)
    );
    for index in 0..24 {
        fs::write(
            originals.path().join(format!("bulk-{index}.jsonl")),
            line.repeat(300 + index * 20),
        )
        .unwrap();
    }
    let names: Vec<String> = fs::read_dir(originals.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
        | 1;
    // Kills spread over the time an uninterrupted migration takes.
    let calibration = DataDir::new();
    for name in &names {
        fs::copy(originals.path().join(name), calibration.path().join(name)).unwrap();
    }
    let started = Instant::now();
    let full = Child::report(Child::spawn(calibration.path(), None));
    let window = started.elapsed().as_millis() as u64;
    assert_eq!(full.report, "OPEN");
    full.release();
    let mut outcomes = HashMap::<&str, usize>::new();
    for run in 0..runs {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let dir = DataDir::new();
        for name in &names {
            fs::copy(originals.path().join(name), dir.path().join(name)).unwrap();
        }
        let mut owner = Child::spawn(dir.path(), None);
        std::thread::sleep(Duration::from_millis(seed % (window + window / 10)));
        owner.kill().unwrap();
        owner.wait().unwrap();

        // What the kill left: no database and untouched sources, a published
        // database and every source in the data dir or legacy/, or a complete
        // one whose sources the first open removes (checked below against the
        // database instead).
        let source = |name: &str| {
            [dir.path().join(name), dir.path().join("legacy").join(name)]
                .into_iter()
                .find(|path| path.exists())
        };
        let outcome = if dir.path().join(DB_FILE).exists() {
            let db = db::Db::open(&dir.path().join(DB_FILE), false).unwrap();
            assert_eq!(db.user_version().unwrap(), SCHEMA_VERSION, "run {run}");
            drop(db);
            if dir.path().join("tcode.db.archive").exists() {
                "published, archival pending"
            } else {
                "complete"
            }
        } else {
            for name in &names {
                assert!(
                    dir.path().join(name).exists(),
                    "run {run}: {name} moved early"
                );
            }
            if dir.path().join("tcode.db.migrating").exists() {
                "staging"
            } else {
                "not started"
            }
        };
        *outcomes.entry(outcome).or_default() += 1;
        for name in names.iter().filter(|_| outcome != "complete") {
            assert_eq!(
                fs::read(source(name).unwrap_or_else(|| panic!("run {run}: {name} lost"))).unwrap(),
                fs::read(originals.path().join(name)).unwrap(),
                "run {run}: {name}"
            );
        }

        let store = dir.migrated();
        for name in names.iter().filter_map(|name| name.strip_suffix(".jsonl")) {
            assert_eq!(
                store.read_event_log(name).unwrap(),
                fs::read(originals.path().join(format!("{name}.jsonl"))).unwrap(),
                "run {run}: {name}"
            );
        }
        assert_eq!(store.read_file().unwrap().sessions.len(), 2, "run {run}");
        store.close().unwrap();
    }
    println!("{runs} kills over a {window} ms migration: {outcomes:?}");
}

/// A Codex log as stored: every turn-changes snapshot carries the turn's whole
/// diff so far, so each one supersedes the one before it on its turn. One
/// legacy bare snapshot and a blank row are among them.
const SNAPSHOT_LOG: &[u8] = b"{\"ts\":1,\"event\":{\"type\":\"turn_started\",\"turn_id\":\"t1\"}}\n\
{\"type\":\"turn_changes_updated\",\"turn_id\":\"t1\",\"changes\":[{\"path\":\"f\",\"kind\":\"modify\",\"diff\":\"-a\\n+b\\n\"}],\"completeness\":\"exact\"}\n\
\n\
{\"ts\":3,\"event\":{\"type\":\"turn_changes_updated\",\"turn_id\":\"t1\",\"changes\":[{\"path\":\"f\",\"kind\":\"modify\",\"diff\":\"-a\\n+c\\n\"}],\"completeness\":\"exact\"}}\n\
{\"ts\":4,\"event\":{\"type\":\"turn_completed\",\"turn_id\":\"t1\",\"status\":\"completed\",\"usage\":null}}\n\
{\"ts\":5,\"event\":{\"type\":\"turn_started\",\"turn_id\":\"t2\"}}\n\
{\"author\":{\"device_id\":\"phone\",\"name\":\"Alice\"},\"ts\":6,\"event\":{\"type\":\"turn_changes_updated\",\"turn_id\":\"t2\",\"changes\":[{\"path\":\"g\",\"kind\":\"create\",\"diff\":\"+d\\n\"}],\"completeness\":\"exact\"}}\n\
{\"ts\":7,\"event\":{\"type\":\"turn_changes_updated\",\"turn_id\":\"t2\",\"changes\":[{\"path\":\"g\",\"kind\":\"create\",\"diff\":\"+e\\n\"}],\"completeness\":\"exact\"}}\n";

fn rows(store: &SessionStore, id: &str) -> Vec<Vec<u8>> {
    store
        .read_event_log(id)
        .unwrap()
        .split_inclusive(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect()
}

/// The pass drops the diffs of exactly the snapshots a later one supersedes,
/// in the form each was stored in, without moving a row; the thread folds as
/// before, and it is not passed again until its rows are replaced or an
/// append leaves a superseded diff it could not name. Rows without a session
/// are never listed.
#[test]
fn superseded_snapshots_lose_their_diffs_in_place_once() {
    let dir = DataDir::new();
    let mut store = dir.store();
    let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/w"), None);
    meta.id = "codex".into();
    store
        .apply(&[
            Mutation::upsert_meta(meta),
            Mutation::replace_event_log("codex", SNAPSHOT_LOG.to_vec()),
            Mutation::replace_event_log("orphan", SNAPSHOT_LOG.to_vec()),
        ])
        .unwrap();
    let before = rows(&store, "codex");
    let folded = tcode_core::session::Timeline::fold_events(store.read_events("codex").unwrap());
    assert_eq!(store.threads_without_diff_pass().unwrap(), ["codex"]);

    let DiffPass::Dropped { rows: dropped, .. } = store.drop_superseded_diffs("codex").unwrap()
    else {
        panic!("superseded snapshots lose their diffs")
    };
    assert_eq!(dropped, 2);
    let after = rows(&store, "codex");
    let mut expected = before.clone();
    expected[1] = b"{\"type\":\"turn_changes_updated\",\"turn_id\":\"t1\",\"changes\":[{\"path\":\"f\",\"kind\":\"modify\",\"diff\":null}],\"completeness\":\"exact\"}\n".to_vec();
    expected[6] = b"{\"author\":{\"device_id\":\"phone\",\"name\":\"Alice\"},\"ts\":6,\"event\":{\"type\":\"turn_changes_updated\",\"turn_id\":\"t2\",\"changes\":[{\"path\":\"g\",\"kind\":\"create\",\"diff\":null}],\"completeness\":\"exact\"}}\n".to_vec();
    assert_eq!(after, expected);
    assert_eq!(
        tcode_core::session::Timeline::fold_events(store.read_events("codex").unwrap()),
        folded
    );
    assert!(store.threads_without_diff_pass().unwrap().is_empty());
    assert_eq!(
        store.drop_superseded_diffs("codex").unwrap(),
        DiffPass::Unchanged
    );

    store.close().unwrap();
    store = dir.store();
    assert!(store.threads_without_diff_pass().unwrap().is_empty());
    store.backfill_thread_activity(&["codex".into()]).unwrap();
    let snapshot = parse_stored_line(
        std::str::from_utf8(
            SNAPSHOT_LOG
                .split_inclusive(|byte| *byte == b'\n')
                .next_back()
                .unwrap(),
        )
        .unwrap(),
    )
    .unwrap()
    .stored
    .event;
    store.append_event("codex", 8, &snapshot).unwrap();
    let snapshots: Vec<_> = store
        .read_events("codex")
        .unwrap()
        .into_iter()
        .filter_map(|record| match record.event {
            AgentEvent::TurnChangesUpdated { changes, .. } => Some(changes[0].diff.is_some()),
            _ => None,
        })
        .collect();
    assert_eq!(snapshots, [false, true, false, false, true]);
    assert!(store.threads_without_diff_pass().unwrap().is_empty());
    let mut fresh = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/w"), None);
    fresh.id = "fresh".into();
    store.upsert_meta(&fresh).unwrap();
    store
        .append_event(
            "fresh",
            1,
            &AgentEvent::TurnStarted {
                turn_id: "fresh".into(),
            },
        )
        .unwrap();
    assert!(store.threads_without_diff_pass().unwrap().is_empty());
    store
        .apply(&[Mutation::replace_event_log("codex", SNAPSHOT_LOG.to_vec())])
        .unwrap();
    assert_eq!(store.threads_without_diff_pass().unwrap(), ["codex"]);
    store.drop_superseded_diffs("codex").unwrap();
}

/// A tolerant read folds without a row it cannot decode, so it could name
/// the wrong snapshots: such a thread keeps every row as it is.
#[test]
fn a_thread_with_an_undecodable_row_keeps_every_row() {
    let dir = DataDir::new();
    let store = dir.store();
    let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/w"), None);
    meta.id = "codex".into();
    store.upsert_meta(&meta).unwrap();
    for (label, bad) in [
        ("unparseable", &b"{not valid json}\n"[..]),
        ("not UTF-8", &b"\xff\xfe{}\n"[..]),
    ] {
        let mut log = SNAPSHOT_LOG.to_vec();
        let at: usize = SNAPSHOT_LOG
            .split_inclusive(|byte| *byte == b'\n')
            .take(5)
            .map(<[u8]>::len)
            .sum();
        log.splice(at..at, bad.iter().copied());
        store
            .apply(&[Mutation::replace_event_log("codex", log.clone())])
            .unwrap();
        assert_eq!(store.threads_without_diff_pass().unwrap(), ["codex"]);
        assert_eq!(
            store.drop_superseded_diffs("codex").unwrap(),
            DiffPass::Undecodable { position: 5 },
            "{label}"
        );
        assert_eq!(store.read_event_log("codex").unwrap(), log, "{label}");
        assert!(
            store.threads_without_diff_pass().unwrap().is_empty(),
            "{label}"
        );
    }
}

#[test]
fn appended_turn_boundaries_match_replay_across_restarts_imports_and_forks() {
    use tcode_core::session::Timeline;
    const LOG: &str = include_str!("fixtures/turn-index.jsonl");
    let dir = DataDir::new();
    let mut store = dir.store();
    let mut fold = Timeline::default();
    let mut starts = Vec::new();
    for (row, line) in LOG.lines().enumerate() {
        let record = parse_stored_line(line).unwrap().stored;
        let turns = fold.turns.len();
        fold.apply_stored(&record);
        if fold.turns.len() > turns {
            starts.push(row as u64);
        }
        store
            .append_event("thread", row as u64, &record.event)
            .unwrap();
        assert_eq!(
            store.turn_index("thread").unwrap(),
            Some(TurnIndex {
                turns: fold.turns.len() as u64,
                starts: starts.clone(),
            }),
            "row {row}"
        );
        if row == 9 {
            store.close().unwrap();
            store = dir.store();
        }
    }
    let expected = TurnIndex {
        turns: 2,
        starts: vec![
            2, 8, 15, 21, 26, 29, 31, 37, 42, 45, 48, 51, 54, 57, 61, 66, 71,
        ],
    };
    assert_eq!(store.turn_index("thread").unwrap(), Some(expected.clone()));
    store
        .apply(&[
            Mutation::replace_event_log("import", LOG.as_bytes().to_vec()),
            Mutation::clone_events("thread", "fork"),
        ])
        .unwrap();
    assert!(store.turn_index("import").unwrap().is_none());
    let mut imported = SessionMeta::new(ProviderKind::Codex, dir.path().to_owned(), None);
    imported.id = "import".into();
    store.upsert_meta(&imported).unwrap();
    store.backfill_thread_activity(&["import".into()]).unwrap();
    for id in ["import", "fork"] {
        assert_eq!(store.turn_index(id).unwrap(), Some(expected.clone()));
    }
    store
        .run("seed an older fold version", |db| {
            db.write(|db, connection| {
                db.execute(
                    connection,
                    "UPDATE turn_index SET fold_version = 0 WHERE session_id = 'thread'",
                    (),
                )?;
                Ok(())
            })
        })
        .unwrap();
    assert!(store.turn_index("thread").unwrap().is_none());
    let mut meta = SessionMeta::new(ProviderKind::Codex, dir.path().to_owned(), None);
    meta.id = "thread".into();
    store.upsert_meta(&meta).unwrap();
    store.backfill_thread_activity(&["thread".into()]).unwrap();
    assert_eq!(store.turn_index("thread").unwrap(), Some(expected));
    store.remove_session("thread").unwrap();
    assert!(store.turn_index("thread").unwrap().is_none());
}

#[test]
fn legacy_codex_file_contents_replay_as_patches_without_rewriting_history() {
    use agent::ItemContent;
    let dir = DataDir::new();
    let store = dir.store();
    let mut meta = SessionMeta::new(ProviderKind::ClaudeCode, PathBuf::from("/w"), None);
    meta.id = "diff-history".into();
    let legacy = concat!(
        "{\"ts\":1,\"event\":{\"type\":\"item_completed\",\"id\":\"add\",\"content\":{\"kind\":\"file_change\",\"status\":\"completed\",\"changes\":[{\"path\":\"a.md\",\"kind\":\"create\",\"diff\":\"- list\\n\"}]}}}\n",
        "{\"type\":\"provider_relay\",\"from_provider\":\"codex\",\"from_model\":null,\"to_provider\":\"claude_code\",\"to_model\":null}\n",
        "{\"type\":\"item_completed\",\"id\":\"claude\",\"content\":{\"kind\":\"file_change\",\"status\":\"completed\",\"changes\":[{\"path\":\"b.md\",\"kind\":\"create\",\"diff\":\"+hello\"}]}}\n"
    );
    store
        .apply(&[
            Mutation::upsert_meta(meta.clone()),
            Mutation::replace_event_log("diff-history", legacy.as_bytes().to_vec()),
        ])
        .unwrap();
    let records = store.read_events("diff-history").unwrap();
    let patch = |event: &AgentEvent| {
        let AgentEvent::ItemCompleted(item) = event else {
            panic!("completed")
        };
        let ItemContent::FileChange { changes, .. } = &item.content else {
            panic!("file change")
        };
        changes[0].diff.clone().unwrap()
    };
    assert_eq!(patch(&records[0].event), "@@ -0,0 +1,1 @@\n+- list\n");
    assert_eq!(patch(&records[2].event), "+hello");
    assert_eq!(
        store.read_rows("diff-history", 0..1).unwrap().records,
        records[..1]
    );
    assert_eq!(
        store.read_rows("diff-history", 2..3).unwrap().records,
        records[2..]
    );
    assert_eq!(
        store.read_event_log("diff-history").unwrap(),
        legacy.as_bytes()
    );
    let exported =
        crate::export::render_thread(&store, &meta, tcode_protocol::ThreadExportFormat::Jsonl)
            .unwrap();
    let export_path = dir.path().join("history.jsonl");
    fs::write(&export_path, exported).unwrap();
    let imported = crate::export::read_tcode_export(&export_path).unwrap();
    assert_eq!(imported.event_log, legacy.as_bytes());
    assert_eq!(imported.events, records);
    let mut codex = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/w"), None);
    codex.id = "new-diffs".into();
    store.upsert_meta(&codex).unwrap();
    store
        .append_event("new-diffs", 4, &records[0].event)
        .unwrap();
    assert_eq!(
        patch(&store.read_events("new-diffs").unwrap()[0].event),
        patch(&records[0].event)
    );
}
