//! Persistence for tcode threads.
//!
//! Projects, thread metadata and every thread's event log live in one Turso
//! database, `tcode.db`, in the data dir ([`data_dir`]):
//!   * `projects` and `sessions` hold each [`Project`] / [`SessionMeta`] as the
//!     JSON serde produces for it.
//!   * `events` holds each thread's log as raw byte segments, one per line
//!     including its `\n`, densely numbered from 0: the bytes of a `{ ts, event }`
//!     record, or whatever a migrated or imported log contained. A turn-changes
//!     snapshot that a later one supersedes is kept without its diffs
//!     ([`tcode_core::session::TurnSnapshots`]).
//!   * `diff_pass` names the threads whose superseded snapshots
//!     [`SessionStore::drop_superseded_diffs`] has dealt with.
//!   * `turn_index_v2` holds append-maintained turn boundaries.
//!   * `kept_worktrees` holds the path of every worktree whose thread was
//!     deleted with the worktree kept.
//!
//! Model and command caches stay as JSON files beside it.
//!
//! Replay accepts timestamped records and legacy bare [`AgentEvent`] lines,
//! then folds [`StoredEvent`]s into a [`tcode_core::session::Timeline`].

mod activity;
mod db;
mod log_read;
mod migrate;
mod projection;
mod relocate;
mod search;
mod superseded;
#[cfg(test)]
mod tests;
mod turn_index;

pub use log_read::LogRead;
pub use migrate::{Migration, MigrationPhase, MigrationProgress};
pub use projection::ProjectionPublication;
pub use search::{IndexedEntry, SearchMatch, SearchSource};
pub use superseded::DiffPass;
pub use turn_index::TurnIndex;

use std::collections::HashMap;
use std::fs::{self, File};
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use agent::{AgentEvent, ModelSpec, ProviderCommand, ProviderKind};
use serde::{Deserialize, Serialize};

use tcode_core::project::{IndexFile, Project, SessionMeta};
use tcode_core::session::{Author, StoredEvent};

use db::{Broken, Db, KEPT_WORKTREES, SCHEMA_VERSION, blob, integer, is_broken};

const DATA_DIR_ENV: &str = "TCODE_DATA_DIR";
const DB_FILE: &str = "tcode.db";
/// Held with an OS file lock for as long as a host owns the data dir, which
/// covers the migration as well as the open database. It is never deleted:
/// unlinking a lock file while another process waits on it would let two
/// owners in.
const LOCK_FILE: &str = "tcode.lock";
/// A macOS permission relaunch starts the new instance while the old one is
/// still closing its store.
const RELAUNCH_WAIT: Duration = Duration::from_secs(15);

/// On-disk envelope wrapping each event with its record time. Kept private:
/// callers deal in [`StoredEvent`] (which tolerates the legacy bare form).
#[derive(Serialize, Deserialize)]
struct EventEnvelope {
    #[serde(default)]
    origin: Option<tcode_core::session::MessageOrigin>,
    #[serde(default)]
    author: Option<Author>,
    ts: u64,
    event: AgentEvent,
    #[serde(default)]
    file_diff_version: Option<u8>,
}

#[derive(Serialize)]
struct EventEnvelopeRef<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<tcode_core::session::MessageOrigin>,
    #[serde(skip_serializing_if = "Option::is_none")]
    author: Option<&'a Author>,
    ts: u64,
    event: &'a AgentEvent,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_diff_version: Option<u8>,
}

impl<'a> EventEnvelopeRef<'a> {
    fn new(
        ts: u64,
        event: &'a AgentEvent,
        author: Option<&'a Author>,
        origin: Option<tcode_core::session::MessageOrigin>,
    ) -> Self {
        let file_change = match event {
            AgentEvent::ItemStarted(item)
            | AgentEvent::ItemUpdated(item)
            | AgentEvent::ItemCompleted(item) => {
                matches!(item.content, agent::ItemContent::FileChange { .. })
            }
            AgentEvent::ApprovalRequested(request) => {
                matches!(request.kind, agent::ApprovalKind::FileChange { .. })
            }
            _ => false,
        };
        Self {
            origin,
            author,
            ts,
            event,
            file_diff_version: file_change.then_some(1),
        }
    }
}

/// Which installation a cached command list belongs to. Native commands
/// depend on the provider's home (its plugins, skills and settings), so two
/// profiles with different homes never seed each other's menus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandsCacheKey {
    /// `home` is the profile's home override; `None` is the CLI's default.
    Native {
        provider: ProviderKind,
        home: Option<PathBuf>,
    },
    Acp {
        agent_id: String,
    },
}

impl CommandsCacheKey {
    /// `None` for an ACP session without an agent id.
    pub fn new(
        provider: ProviderKind,
        home: Option<PathBuf>,
        acp_agent_id: Option<&str>,
    ) -> Option<Self> {
        match provider {
            ProviderKind::Acp => acp_agent_id.map(|agent_id| Self::Acp {
                agent_id: agent_id.to_string(),
            }),
            provider => Some(Self::Native { provider, home }),
        }
    }
}

/// 64-bit FNV-1a: a short, stable file-name segment for a home path.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Cheap, cloneable handle to the data directory. Every handle for the same
/// directory in this process shares one database, writer and lifecycle.
#[derive(Clone)]
pub struct SessionStore {
    root: PathBuf,
    /// An older build's data dir that moves into `root` before the store
    /// opens ([`SessionStore::migrate`]).
    previous: Option<PathBuf>,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for SessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStore")
            .field("root", &self.root)
            .field("previous", &self.previous)
            .finish_non_exhaustive()
    }
}

/// The data dir: `TCODE_DATA_DIR` when it is set — a throwaway profile (its
/// own sessions, settings and installed ACP agents) for demos and screenshots —
/// else `~/.tcode` on every platform.
pub fn data_dir() -> io::Result<PathBuf> {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV).filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    dirs::home_dir()
        .map(|home| home.join(".tcode"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "no home directory to keep Tcode's data in (~/.tcode); set {DATA_DIR_ENV} to \
                     choose one"
                ),
            )
        })
}

struct Shared {
    state: Mutex<State>,
    /// Signalled whenever an operation finishes, for [`SessionStore::close`].
    idle: Condvar,
    /// Per-session event-log generation, advanced after every committed
    /// change to that log; see [`SessionStore::event_generation`].
    generations: Mutex<HashMap<String, u64>>,
    next_generation: AtomicU64,
    /// Full event-log parses, shared by every clone of this store so tests can
    /// prove a caller served history from memory.
    #[cfg(any(test, feature = "test-support"))]
    event_reads: std::sync::atomic::AtomicUsize,
}

enum State {
    /// Opened on first use, so a handle that only needs the directory (a
    /// subcommand, a settings file) never takes the data dir.
    Unopened,
    /// The data dir is owned, for a migration and then the database, but the
    /// database is not open yet.
    Owned(File),
    /// [`SessionStore::migrate`] holds the ownership lock while it runs.
    Migrating,
    Open(Live),
    /// [`SessionStore::close`] is waiting for in-flight operations.
    Closing(Live),
    Closed,
    /// A panic or an unrecoverable connection: nothing is served again, and
    /// the ownership lock stays held while any handle remains, so no other
    /// process opens a database this one may have left mid-operation.
    Failed {
        reason: String,
        _ownership: Option<File>,
    },
}

struct Live {
    db: Arc<Db>,
    ownership: File,
    in_flight: usize,
}

/// Every open handle by canonical data dir, so a second
/// [`SessionStore::open_at`] in one process shares the first one's database
/// instead of contending for its locks.
static STORES: LazyLock<Mutex<HashMap<PathBuf, Weak<Shared>>>> = LazyLock::new(Default::default);

/// One change to the store. [`SessionStore::apply`] commits a sequence of them
/// in a single transaction.
#[derive(Debug, Clone)]
pub struct Mutation(Op);

#[derive(Debug, Clone)]
enum Op {
    AppendEvent {
        session_id: String,
        line: Vec<u8>,
        projection: Option<(String, Vec<u8>)>,
        record: Box<StoredEvent>,
    },
    ReplaceEventLog {
        session_id: String,
        bytes: Vec<u8>,
    },
    CloneEvents {
        src: String,
        dst: String,
    },
    UpsertMeta(Box<SessionMeta>),
    UpsertProject(Box<Project>),
    RemoveSession(String),
    RemoveProject(String),
    KeepWorktree(PathBuf),
}

impl Mutation {
    /// Append one event, wrapped in a timestamped envelope
    /// (`{"ts": <unix_ms>, "event": {…}}`).
    pub fn append_event(session_id: &str, ts: u64, event: &AgentEvent) -> io::Result<Self> {
        Self::append_authored_event(session_id, ts, event, None, None, |_| None)
    }

    /// `project` returns a derived body using the same policy/version as
    /// [`LogRead::visit_projected_rows`]; it never replaces the canonical event.
    pub fn append_authored_event(
        session_id: &str,
        ts: u64,
        event: &AgentEvent,
        author: Option<&Author>,
        origin: Option<tcode_core::session::MessageOrigin>,
        project: impl FnOnce(usize) -> Option<(String, Vec<u8>)>,
    ) -> io::Result<Self> {
        let mut line = serde_json::to_vec(&EventEnvelopeRef::new(ts, event, author, origin))
            .map_err(invalid_data)?;
        line.push(b'\n');
        let projection = project(line.len());
        Ok(Self(Op::AppendEvent {
            session_id: session_id.to_owned(),
            line,
            projection,
            record: Box::new(StoredEvent {
                ts: Some(ts),
                event: event.clone(),
                author: author.cloned(),
                origin,
                elided: None,
            }),
        }))
    }

    /// Replace a session's whole native event log with `bytes`, kept exactly.
    pub fn replace_event_log(session_id: &str, bytes: Vec<u8>) -> Self {
        Self(Op::ReplaceEventLog {
            session_id: session_id.to_owned(),
            bytes,
        })
    }

    /// Give `dst` a copy of `src`'s event log. A missing source is an empty
    /// transcript.
    pub fn clone_events(src: &str, dst: &str) -> Self {
        Self(Op::CloneEvents {
            src: src.to_owned(),
            dst: dst.to_owned(),
        })
    }

    /// Insert or replace a meta (by id).
    pub fn upsert_meta(meta: SessionMeta) -> Self {
        Self(Op::UpsertMeta(Box::new(meta)))
    }

    /// Insert or replace a project; its previous managed icon is removed once
    /// the change has committed.
    pub fn upsert_project(project: Project) -> Self {
        Self(Op::UpsertProject(Box::new(project)))
    }

    /// Remove a session's meta and its event log.
    pub fn remove_session(id: &str) -> Self {
        Self(Op::RemoveSession(id.to_owned()))
    }

    /// Remove a project; its managed icon is removed once the change has
    /// committed. Sessions are removed separately.
    pub fn remove_project(id: &str) -> Self {
        Self(Op::RemoveProject(id.to_owned()))
    }

    /// Record that the worktree at `path` outlives its deleted thread.
    pub fn keep_worktree(path: &Path) -> Self {
        Self(Op::KeepWorktree(path.to_owned()))
    }

    /// Approximate bytes this change writes, for bounding a batch.
    pub fn payload_len(&self) -> usize {
        match &self.0 {
            Op::AppendEvent {
                line, projection, ..
            } => line.len() + projection.as_ref().map_or(0, |(_, body)| body.len()),
            Op::ReplaceEventLog { bytes, .. } => bytes.len(),
            _ => 0,
        }
    }

    /// The session whose event log this change rewrites, if any.
    fn event_log(&self) -> Option<&str> {
        match &self.0 {
            Op::AppendEvent { session_id, .. } | Op::ReplaceEventLog { session_id, .. } => {
                Some(session_id)
            }
            Op::CloneEvents { dst, .. } => Some(dst),
            Op::RemoveSession(id) => Some(id),
            Op::UpsertMeta(_)
            | Op::UpsertProject(_)
            | Op::RemoveProject(_)
            | Op::KeepWorktree(_) => None,
        }
    }
}

impl SessionStore {
    /// A host's store: in `root` when one is given (`tcode-headless
    /// --data-dir`), else in [`data_dir`]. An older build's data dir moves in
    /// first, from `LEGACY_TCODE_DATA_DIR` when it is set, else — unless the
    /// data dir was chosen explicitly — from the platform data dir an older
    /// build used; see [`SessionStore::needs_migration`].
    pub fn open_host(root: Option<PathBuf>) -> io::Result<Self> {
        let explicit =
            root.is_some() || std::env::var_os(DATA_DIR_ENV).is_some_and(|dir| !dir.is_empty());
        let root = match root {
            Some(root) => root,
            None => data_dir()?,
        };
        let mut store = Self::open_at(root)?;
        store.previous = relocate::source(explicit);
        Ok(store)
    }

    /// A handle to `root`, created if needed, that never moves an older data
    /// dir in. The database is not opened until [`SessionStore::open`] or the
    /// first operation that needs it.
    pub fn open_at(root: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&root)?;
        let key = fs::canonicalize(&root)?;
        let mut stores = STORES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        stores.retain(|_, shared| shared.strong_count() > 0);
        let shared = match stores.get(&key).and_then(Weak::upgrade) {
            Some(shared) if !shared.finished() => shared,
            _ => {
                let shared = Arc::new(Shared {
                    state: Mutex::new(State::Unopened),
                    idle: Condvar::new(),
                    generations: Mutex::default(),
                    next_generation: AtomicU64::new(1),
                    #[cfg(any(test, feature = "test-support"))]
                    event_reads: Default::default(),
                });
                stores.insert(key, Arc::downgrade(&shared));
                shared
            }
        };
        Ok(Self {
            root,
            previous: None,
            shared,
        })
    }

    /// How many times [`SessionStore::read_events`] parsed a log through this
    /// store or any of its clones.
    #[cfg(any(test, feature = "test-support"))]
    pub fn event_reads(&self) -> usize {
        self.shared
            .event_reads
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    /// Take ownership of the data dir and open its database now. A data dir
    /// that still needs migrating ([`SessionStore::needs_migration`]) is
    /// refused. Fails with
    /// [`io::ErrorKind::ResourceBusy`] while another host owns the directory.
    pub fn open(&self) -> io::Result<()> {
        self.run("open", |_| Ok(()))
    }

    /// Take ownership of the data dir, keeping it for the database opened
    /// later, and say whether [`SessionStore::migrate`] must prepare it before
    /// the store opens: an older build's data dir is still to move in, or the
    /// data dir holds the JSON index or JSONL logs of an older build that must
    /// move into `tcode.db`. Fails with [`io::ErrorKind::ResourceBusy`] while
    /// another host owns the directory.
    pub fn needs_migration(&self) -> io::Result<bool> {
        let mut state = self.shared.lock_state()?;
        match &*state {
            State::Unopened => *state = State::Owned(acquire_ownership(&self.root)?),
            State::Owned(_) => {}
            State::Open(_) => return Ok(false),
            other => return Err(unavailable(other, "check for a migration")),
        }
        Ok(relocate::needed(&self.root, self.previous.as_deref())? || migrate::needed(&self.root)?)
    }

    /// The older data dir still to move into this one, without taking
    /// ownership: whatever is written to the data dir before the move would
    /// stop it with a collision.
    pub fn pending_relocation(&self) -> io::Result<Option<&Path>> {
        Ok(match self.previous.as_deref() {
            Some(previous) if relocate::needed(&self.root, Some(previous))? => Some(previous),
            _ => None,
        })
    }

    /// Prepare the data dir under its ownership lock, which the store keeps
    /// for the database afterwards: first move an older build's data dir in
    /// ([`MigrationPhase::Relocating`]), then migrate an older build's JSON
    /// index and JSONL logs into `tcode.db`. `progress` is called after every
    /// entry moved, every chunk of about 8 MiB copied or imported, and every
    /// log; `cancel` is checked at the same points and between phases until
    /// the migrated database is complete. A cancelled move keeps the entries
    /// it moved and is continued by the next start; a cancelled migration
    /// removes its staging files and changes nothing else, and a later one
    /// starts over.
    pub fn migrate(
        &self,
        mut progress: impl FnMut(MigrationProgress),
        cancel: &AtomicBool,
    ) -> io::Result<Migration> {
        let ownership = {
            let mut state = self.shared.lock_state()?;
            match std::mem::replace(&mut *state, State::Migrating) {
                State::Unopened => match acquire_ownership(&self.root) {
                    Ok(ownership) => ownership,
                    Err(error) => {
                        *state = State::Unopened;
                        return Err(error);
                    }
                },
                State::Owned(ownership) => ownership,
                State::Open(live) => {
                    *state = State::Open(live);
                    return Ok(Migration::Completed);
                }
                other => {
                    let error = unavailable(&other, "migrate");
                    *state = other;
                    return Err(error);
                }
            }
        };
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            match relocate::run(&self.root, self.previous.as_deref(), &mut progress, cancel)? {
                Migration::Cancelled => Ok(Migration::Cancelled),
                Migration::Completed => migrate::run(&self.root, &mut progress, cancel),
            }
        }));
        let mut state = self.shared.lock_state()?;
        let result = match outcome {
            Ok(result) => {
                *state = State::Owned(ownership);
                result
            }
            Err(panic) => {
                let reason = format!(
                    "session store panicked while migrating {}: {}",
                    self.root.display(),
                    panic_message(panic.as_ref())
                );
                log::error!("{reason}");
                *state = State::Failed {
                    reason: reason.clone(),
                    _ownership: Some(ownership),
                };
                Err(io::Error::other(reason))
            }
        };
        self.shared.idle.notify_all();
        result
    }

    /// Whether the store stopped serving after a panic or a broken connection.
    pub fn is_failed(&self) -> bool {
        matches!(
            *self
                .shared
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            State::Failed { .. }
        )
    }

    /// Wait for every in-flight operation and a running migration, checkpoint
    /// the WAL into the main file and release the database and the data dir.
    /// Every handle of this store fails afterwards; a store that already
    /// failed is not checkpointed.
    pub fn close(&self) -> io::Result<()> {
        let mut state = self.shared.lock_state()?;
        loop {
            match std::mem::replace(&mut *state, State::Closed) {
                State::Unopened | State::Closed => return Ok(()),
                State::Owned(ownership) => return ownership.unlock(),
                State::Migrating => {
                    *state = State::Migrating;
                    state = self
                        .shared
                        .idle
                        .wait(state)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                State::Open(live) => {
                    live.db.cancel_reads();
                    *state = State::Closing(live);
                }
                State::Closing(live) if live.in_flight > 0 => {
                    *state = State::Closing(live);
                    state = self
                        .shared
                        .idle
                        .wait(state)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                State::Closing(live) => {
                    let Live { db, ownership, .. } = live;
                    db.close_readers();
                    let result = match Arc::try_unwrap(db) {
                        Ok(db) => catch_unwind(AssertUnwindSafe(|| db.checkpoint()))
                            .unwrap_or_else(|panic| {
                                Err(io::Error::other(format!(
                                    "session store panicked during the closing checkpoint: {}",
                                    panic_message(panic.as_ref())
                                )))
                            }),
                        Err(_) => Err(io::Error::other(
                            "session store is still referenced after its last operation",
                        )),
                    };
                    // A concurrently launching child can inherit the lock descriptor
                    // until exec. Closing our copy alone leaves that child holding it.
                    let result = result.and(ownership.unlock());
                    drop(ownership);
                    return result;
                }
                failed @ State::Failed { .. } => {
                    let error = failed_error(&failed);
                    *state = failed;
                    return Err(error);
                }
            }
        }
    }

    /// Run `body` against the open database. A panic inside it (Turso asserts
    /// its invariants by panicking) becomes an error naming `operation` and
    /// fails the store for every handle.
    fn run<R>(&self, operation: &str, body: impl FnOnce(&Db) -> io::Result<R>) -> io::Result<R> {
        let db = self.acquire(operation)?;
        let outcome = catch_unwind(AssertUnwindSafe(|| body(&db)));
        drop(db);
        let result = match outcome {
            Ok(Err(error)) if is_broken(&error) => {
                self.shared.fail(format!("{operation}: {error}"));
                Err(error)
            }
            Ok(result) => result,
            Err(panic) => {
                let reason = format!(
                    "session store panicked during {operation}: {}",
                    panic_message(panic.as_ref())
                );
                log::error!("{reason}");
                self.shared.fail(reason.clone());
                Err(io::Error::other(reason))
            }
        };
        self.shared.release();
        result
    }

    fn acquire(&self, operation: &str) -> io::Result<Arc<Db>> {
        let mut state = self.shared.lock_state()?;
        let ownership = match std::mem::replace(&mut *state, State::Unopened) {
            State::Unopened => Some(acquire_ownership(&self.root)?),
            State::Owned(ownership) => Some(ownership),
            other => {
                *state = other;
                None
            }
        };
        if let Some(ownership) = ownership {
            // Dropping the ownership on failure lets the next attempt start over.
            match catch_unwind(AssertUnwindSafe(|| {
                open_live(&self.root, self.previous.as_deref(), ownership)
            })) {
                Ok(Ok(live)) => *state = State::Open(live),
                Ok(Err(error)) => return Err(error),
                Err(panic) => {
                    let reason = format!(
                        "session store panicked while opening {}: {}",
                        self.root.display(),
                        panic_message(panic.as_ref())
                    );
                    log::error!("{reason}");
                    *state = State::Failed {
                        reason: reason.clone(),
                        _ownership: None,
                    };
                    return Err(io::Error::other(reason));
                }
            }
        }
        match &mut *state {
            State::Open(live) => {
                live.in_flight += 1;
                Ok(live.db.clone())
            }
            other => Err(unavailable(other, operation)),
        }
    }

    /// Commit `mutations` in one transaction, then run their deferred side
    /// effects. Either every change is committed or none is.
    pub fn apply(&self, mutations: &[Mutation]) -> io::Result<()> {
        if mutations.is_empty() {
            return Ok(());
        }
        let icons = self.run("write", |db| {
            db.write(|db, connection| {
                let mut icons = Vec::new();
                let mut indices = turn_index::Batch::new();
                for mutation in mutations {
                    if !matches!(mutation.0, Op::AppendEvent { .. }) {
                        turn_index::flush(db, connection, &indices)?;
                        indices.clear();
                    }
                    apply_op(db, connection, &mutation.0, &mut icons, &mut indices)?;
                }
                turn_index::flush(db, connection, &indices)?;
                Ok(icons)
            })
        })?;
        self.advance_event_generations(mutations.iter().filter_map(Mutation::event_log));
        for icon in icons {
            self.remove_project_icon(icon);
        }
        Ok(())
    }

    fn advance_event_generations<'a>(&self, ids: impl IntoIterator<Item = &'a str>) {
        let mut generations = self
            .shared
            .generations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for id in ids {
            let generation = self.shared.next_generation.fetch_add(1, Ordering::Relaxed);
            generations.insert(id.to_owned(), generation);
        }
    }

    /// A value that changes whenever a committed write changes `id`'s event
    /// log through this store, so a cache of something derived from the log
    /// can tell it is stale. 0 means unchanged since the store was opened.
    pub fn event_generation(&self, id: &str) -> u64 {
        self.shared
            .generations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .copied()
            .unwrap_or(0)
    }

    /// Read a native event log back as the exact bytes it holds.
    pub fn read_event_log(&self, id: &str) -> io::Result<Vec<u8>> {
        self.run("read event log", |db| {
            db.read(|db, connection| {
                let mut bytes = Vec::new();
                db.query(
                    connection,
                    "SELECT line FROM events WHERE session_id = ?1 ORDER BY position",
                    (id,),
                    |row| {
                        bytes.extend_from_slice(&blob(row, 0)?);
                        Ok(())
                    },
                )?;
                Ok(bytes)
            })
        })
    }

    fn models_path(&self, provider: ProviderKind) -> PathBuf {
        let name = match provider {
            ProviderKind::Codex => "codex",
            ProviderKind::ClaudeCode => "claude",
            ProviderKind::Pi => "pi",
            ProviderKind::OpenCode => "opencode",
            ProviderKind::Cursor => "cursor",
            ProviderKind::Grok => "grok",
            // ACP agents publish their models over the wire at session start
            // (`AgentEvent::ProviderOptions`), so there is no catalog to cache.
            ProviderKind::Acp => "acp",
        };
        self.root.join(format!("models-{name}.json"))
    }

    fn commands_path(&self, key: &CommandsCacheKey) -> PathBuf {
        let name = match key {
            CommandsCacheKey::Native { provider, home } => {
                let provider = match provider {
                    ProviderKind::Codex => "codex",
                    ProviderKind::ClaudeCode => "claude",
                    ProviderKind::Pi => "pi",
                    ProviderKind::OpenCode => "opencode",
                    ProviderKind::Cursor => "cursor",
                    ProviderKind::Grok => "grok",
                    ProviderKind::Acp => "acp",
                };
                let home = home.as_deref().map_or_else(
                    || "default".to_string(),
                    |home| format!("{:016x}", fnv1a(home.as_os_str().as_encoded_bytes())),
                );
                format!("{provider}-{home}")
            }
            CommandsCacheKey::Acp { agent_id } => {
                // Registry ids are external input and may contain path separators.
                // Hex keeps the filename reversible and collision-free without
                // allowing an id to escape the data directory.
                let encoded = agent_id
                    .as_bytes()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                format!("acp-{encoded}")
            }
        };
        self.root.join(format!("commands-{name}.json"))
    }

    /// Load the last-fetched model catalog for `provider` so the picker is
    /// instant offline. Empty when never fetched / unreadable.
    pub fn load_models(&self, provider: ProviderKind) -> Vec<ModelSpec> {
        let Ok(bytes) = fs::read(self.models_path(provider)) else {
            return Vec::new();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    /// Persist the freshly fetched model catalog for `provider`.
    pub fn save_models(&self, provider: ProviderKind, models: &[ModelSpec]) -> io::Result<()> {
        let path = self.models_path(provider);
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(models).map_err(invalid_data)?;
        fs::write(&tmp, data)?;
        fs::rename(&tmp, path)
    }

    /// Load the most recently reported command/skill list for one native
    /// installation or ACP agent. Empty when missing or unreadable.
    pub fn load_commands(&self, key: &CommandsCacheKey) -> Vec<ProviderCommand> {
        let Ok(bytes) = fs::read(self.commands_path(key)) else {
            return Vec::new();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    /// Atomically persist the complete command/skill list reported by one
    /// native installation or ACP agent. Empty lists are meaningful: they
    /// replace a stale non-empty cache.
    pub fn save_commands(
        &self,
        key: &CommandsCacheKey,
        commands: &[ProviderCommand],
    ) -> io::Result<()> {
        let path = self.commands_path(key);
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(commands).map_err(invalid_data)?;
        fs::write(&tmp, data)?;
        fs::rename(&tmp, path)
    }

    /// Forget a cached command list after the installation changed under it.
    pub fn invalidate_commands(&self, key: &CommandsCacheKey) -> io::Result<()> {
        match fs::remove_file(self.commands_path(key)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    /// Load every project and session, in the order they were first stored.
    /// A row this build cannot decode is skipped with a warning and left in
    /// the database untouched.
    pub fn read_file(&self) -> io::Result<IndexFile> {
        self.run("read index", |db| {
            db.read(|db, connection| {
                // One read transaction, so projects and sessions come from the
                // same snapshot.
                db.execute(connection, "BEGIN", ())?;
                let file = read_index(db, connection);
                db.execute(connection, "COMMIT", ())?;
                file
            })
        })
    }

    /// Load the session index (newest first).
    pub fn load_index(&self) -> io::Result<Vec<SessionMeta>> {
        let mut metas = self.read_file()?.sessions;
        metas.sort_by_key(|b| std::cmp::Reverse(b.updated_at));
        Ok(metas)
    }

    /// Insert or replace a meta in the index (by id).
    pub fn upsert_meta(&self, meta: &SessionMeta) -> io::Result<()> {
        self.apply(&[Mutation::upsert_meta(meta.clone())])
    }

    /// Insert or replace a project, then remove its previous managed icon.
    pub fn upsert_project(&self, project: &Project) -> io::Result<()> {
        self.apply(&[Mutation::upsert_project(project.clone())])
    }

    /// Remove a project and its managed icon. Sessions are removed separately.
    pub fn remove_project(&self, id: &str) -> io::Result<()> {
        self.apply(&[Mutation::remove_project(id)])
    }

    fn remove_project_icon(&self, path: Option<PathBuf>) {
        if let Some(path) = path
            && path.parent() == Some(self.root.join("project-icons").as_path())
            && let Err(error) = fs::remove_file(&path)
            && error.kind() != io::ErrorKind::NotFound
        {
            log::warn!("could not remove project icon {}: {error}", path.display());
        }
    }

    /// Append one event to the session's log, wrapped in a timestamped
    /// envelope (`{"ts": <unix_ms>, "event": {…}}`).
    pub fn append_event(&self, id: &str, ts: u64, event: &AgentEvent) -> io::Result<()> {
        self.apply(&[Mutation::append_event(id, ts, event)?])
    }

    /// Read and parse every persisted event for a session, skipping lines that
    /// do not parse.
    ///
    /// Each line is tolerantly parsed as either a timestamped envelope
    /// (`{"ts":…,"event":…}`) or a legacy bare event (`{"type":…}`), so logs
    /// written before the envelope format still replay (with `ts == None`).
    pub fn read_events(&self, id: &str) -> io::Result<Vec<StoredEvent>> {
        self.read_log(id).map(|log| log.records)
    }

    /// [`SessionStore::read_events`] with the stored row each record came
    /// from and what the read skipped.
    pub fn read_log(&self, id: &str) -> io::Result<EventLog> {
        self.read_log_until(id, u64::MAX)
    }

    /// [`SessionStore::read_log`] of the rows before position `end` alone:
    /// what the log held when [`SessionStore::next_row`] returned `end`, since
    /// rows are only ever appended.
    pub fn read_log_until(&self, id: &str, end: u64) -> io::Result<EventLog> {
        #[cfg(any(test, feature = "test-support"))]
        self.shared
            .event_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.read_rows(id, 0..end)
    }

    /// The records of the rows at `positions` of a session's log, without
    /// reading the rest of it.
    pub fn read_rows(&self, id: &str, positions: std::ops::Range<u64>) -> io::Result<EventLog> {
        let bound = |position: u64| i64::try_from(position).unwrap_or(i64::MAX);
        self.run("read events", |db| {
            db.read(|db, connection| {
                let mut log = EventLog::default();
                let mut provider = None;
                db.query(
                    connection,
                    "SELECT position, line FROM events \
                     WHERE session_id = ?1 AND position >= ?2 AND position < ?3 ORDER BY position",
                    (id, bound(positions.start), bound(positions.end)),
                    |row| {
                        let position = integer(row, 0)?;
                        let line = blob(row, 1)?;
                        log.next_row = position as u64 + 1;
                        match decode_row(&line) {
                            Ok(Some(record)) => {
                                if record.legacy_file_diff && provider.is_none() {
                                    provider = stored_provider_at(db, connection, id, position)?;
                                }
                                let stored = record.into_stored(provider);
                                if let AgentEvent::ProviderRelay { to_provider, .. } = &stored.event
                                {
                                    provider = Some(*to_provider);
                                }
                                log.records.push(stored);
                                log.rows.push(position as u64);
                            }
                            Ok(None) => {}
                            Err(reason) => {
                                log.undecodable += 1;
                                log::warn!("skipping event {position} of {id}: {reason}");
                            }
                        }
                        Ok(())
                    },
                )?;
                Ok(log)
            })
        })
    }

    /// Remove a session from the index and delete its event log.
    pub fn remove_session(&self, id: &str) -> io::Result<()> {
        self.apply(&[Mutation::remove_session(id)])
    }

    /// Every worktree recorded by [`Mutation::keep_worktree`].
    pub fn kept_worktrees(&self) -> io::Result<Vec<PathBuf>> {
        self.run("read kept worktrees", |db| {
            db.read(|db, connection| {
                let mut paths = Vec::new();
                db.query(connection, "SELECT path FROM kept_worktrees", (), |row| {
                    paths.push(PathBuf::from(db::text(row, 0)?));
                    Ok(())
                })?;
                Ok(paths)
            })
        })
    }
}

impl Shared {
    fn lock_state(&self) -> io::Result<MutexGuard<'_, State>> {
        // The state is replaced whole under the lock, so a poisoned guard
        // still holds a consistent value.
        Ok(self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()))
    }

    fn finished(&self) -> bool {
        matches!(
            *self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            State::Closed | State::Failed { .. }
        )
    }

    fn release(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let State::Open(live) | State::Closing(live) = &mut *state {
            live.in_flight -= 1;
        }
        self.idle.notify_all();
    }

    fn fail(&self, reason: String) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let ownership = match std::mem::replace(&mut *state, State::Closed) {
            State::Open(live) | State::Closing(live) => Some(live.ownership),
            State::Owned(ownership) => Some(ownership),
            State::Failed { _ownership, .. } => _ownership,
            State::Unopened | State::Migrating | State::Closed => None,
        };
        *state = State::Failed {
            reason,
            _ownership: ownership,
        };
        self.idle.notify_all();
    }
}

fn failed_error(state: &State) -> io::Error {
    let State::Failed { reason, .. } = state else {
        unreachable!("only called for a failed store")
    };
    io::Error::other(Broken(format!(
        "the session store stopped after an internal failure ({reason}); restart Tcode"
    )))
}

/// Why a store in `state` cannot `operation` now.
fn unavailable(state: &State, operation: &str) -> io::Error {
    match state {
        State::Migrating => io::Error::new(
            io::ErrorKind::ResourceBusy,
            format!("cannot {operation}: the data directory is being migrated"),
        ),
        failed @ State::Failed { .. } => failed_error(failed),
        _ => io::Error::new(
            io::ErrorKind::BrokenPipe,
            format!("cannot {operation}: the session store is closed"),
        ),
    }
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

/// Bring `tcode.db` up to date in the data dir `ownership` holds, and open it.
/// A data dir an older one has still to move into is refused, as opening it
/// would create the data that makes the move look done.
fn open_live(root: &Path, previous: Option<&Path>, ownership: File) -> io::Result<Live> {
    if let Some(previous) = previous
        && relocate::needed(root, Some(previous))?
    {
        return Err(io::Error::other(format!(
            "{} has not been moved into {} yet",
            previous.display(),
            root.display()
        )));
    }
    migrate::prepare(root)?;
    let path = root.join(DB_FILE);
    let db = Db::open(&path, false)?;
    match db.user_version()? {
        SCHEMA_VERSION => {}
        0 => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} was never completed (user_version 0). It is kept as it is and can be \
                     examined with the sqlite3 command-line tool.",
                    path.display()
                ),
            ));
        }
        version => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} has schema version {version}, written by a newer Tcode; this build \
                     understands version {SCHEMA_VERSION}",
                    path.display()
                ),
            ));
        }
    }
    db.write(|db, connection| {
        db.execute(connection, KEPT_WORKTREES, ())?;
        db.execute(connection, superseded::DIFF_PASS_TABLE, ())?;
        turn_index::create_tables(db, connection)?;
        search::create_tables(db, connection)?;
        activity::create_table(db, connection)?;
        projection::create_cache(db, connection)
    })?;
    migrate::warn_about_stray_sources(root);
    migrate::remove_legacy(root);
    Ok(Live {
        db: Arc::new(db),
        ownership,
        in_flight: 0,
    })
}

/// A session's event log as one read of its rows saw it.
#[derive(Debug, Default)]
pub struct EventLog {
    pub records: Vec<StoredEvent>,
    /// The position of the row each record was read from.
    pub rows: Vec<u64>,
    /// The position after the last row read.
    pub next_row: u64,
    /// Rows that hold no record this build reads and are not blank.
    pub undecodable: usize,
}

fn decode_row(line: &[u8]) -> Result<Option<ParsedRecord>, String> {
    let Ok(line) = std::str::from_utf8(line) else {
        return Err("not UTF-8".into());
    };
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    parse_stored_line(trimmed)
        .map(Some)
        .map_err(|error| format!("unparseable: {error}"))
}

fn acquire_ownership(root: &Path) -> io::Result<File> {
    let path = root.join(LOCK_FILE);
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    let mut deadline = None;
    let mut delay = Duration::from_millis(20);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(fs::TryLockError::Error(error)) => return Err(error),
            Err(fs::TryLockError::WouldBlock) => {
                let deadline = *deadline.get_or_insert_with(|| {
                    // Peek only: the marker is consumed by the host after it
                    // owns the data dir.
                    crate::relaunch::pending(root).then(|| Instant::now() + RELAUNCH_WAIT)
                });
                match deadline {
                    Some(deadline) if Instant::now() < deadline => {
                        std::thread::sleep(delay);
                        delay = (delay * 2).min(Duration::from_millis(250));
                    }
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::ResourceBusy,
                            format!(
                                "another Tcode host is already using the data directory {}; \
                                 quit it first, or set TCODE_DATA_DIR to use another directory",
                                root.display()
                            ),
                        ));
                    }
                }
            }
        }
    }
}

fn read_index(db: &Db, connection: &turso::Connection) -> io::Result<IndexFile> {
    fn rows<T: serde::de::DeserializeOwned>(
        db: &Db,
        connection: &turso::Connection,
        table: &str,
    ) -> io::Result<Vec<T>> {
        let mut values = Vec::new();
        db.query(
            connection,
            &format!("SELECT id, body FROM {table} ORDER BY rowid"),
            (),
            |row| {
                let body = blob(row, 1)?;
                match serde_json::from_slice(&body) {
                    Ok(value) => values.push(value),
                    Err(error) => log::warn!(
                        "skipping {table} row {:?} this build cannot read: {error}",
                        db::text(row, 0).unwrap_or_default()
                    ),
                }
                Ok(())
            },
        )?;
        Ok(values)
    }
    let mut sessions: Vec<SessionMeta> = rows(db, connection, "sessions")?;
    for meta in &mut sessions {
        meta.migrate_lifecycle();
    }
    Ok(IndexFile {
        projects: rows(db, connection, "projects")?,
        sessions,
    })
}

/// The raw segments of a native log: every line with its `\n`, and an
/// unterminated last line as it is.
fn segments(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.split_inclusive(|byte| *byte == b'\n')
}

fn insert_segments(
    db: &Db,
    connection: &turso::Connection,
    session_id: &str,
    first: i64,
    lines: impl IntoIterator<Item = impl AsRef<[u8]>>,
) -> io::Result<i64> {
    let mut position = first;
    for line in lines {
        db.execute(
            connection,
            "INSERT INTO events (session_id, position, line) VALUES (?1, ?2, ?3)",
            (session_id, position, line.as_ref()),
        )?;
        position += 1;
    }
    Ok(position)
}

fn apply_op(
    db: &Db,
    connection: &turso::Connection,
    op: &Op,
    icons: &mut Vec<Option<PathBuf>>,
    indices: &mut turn_index::Batch,
) -> io::Result<()> {
    match op {
        Op::AppendEvent {
            session_id,
            line,
            projection,
            record,
        } => {
            let mut last = None;
            db.query(
                connection,
                "SELECT position, substr(line, -1) = x'0a' FROM events \
                 WHERE session_id = ?1 ORDER BY position DESC LIMIT 1",
                (session_id.as_str(),),
                |row| {
                    last = Some((integer(row, 0)?, integer(row, 1)? != 0));
                    Ok(())
                },
            )?;
            let next = match last {
                None => 0,
                Some((position, terminated)) => {
                    if !terminated {
                        // A log imported without a final newline: terminate
                        // its last line so the new record starts a line.
                        let mut unterminated = Vec::new();
                        db.query(
                            connection,
                            "SELECT line FROM events WHERE session_id = ?1 AND position = ?2",
                            (session_id.as_str(), position),
                            |row| {
                                unterminated = blob(row, 0)?;
                                Ok(())
                            },
                        )?;
                        unterminated.push(b'\n');
                        db.execute(
                            connection,
                            "UPDATE events SET line = ?3 WHERE session_id = ?1 AND position = ?2",
                            (session_id.as_str(), position, unterminated),
                        )?;
                    }
                    position + 1
                }
            };
            insert_segments(db, connection, session_id, next, [line])?;
            if tcode_core::settlement::ThreadActivity::moves_clock(&record.event) {
                activity::append(db, connection, session_id, record)?;
            }
            turn_index::append(db, connection, indices, session_id, next as u64, record)?;
            if let Some((version, body)) = projection
                && let Err(error) = db.execute(
                    connection,
                    "INSERT INTO event_preview_v1 (session_id, position, version, token, body) VALUES (?1, ?2, ?3, '', ?4)",
                    (session_id.as_str(), next, version.as_str(), body.as_slice()),
                )
            {
                log::warn!("could not persist appended history preview: {error}");
            }
        }
        Op::ReplaceEventLog { session_id, bytes } => {
            turn_index::replace_revision(db, connection, session_id)?;
            db.execute(
                connection,
                "DELETE FROM events WHERE session_id = ?1",
                (session_id.as_str(),),
            )?;
            insert_segments(db, connection, session_id, 0, segments(bytes))?;
            activity::recompute(db, connection, session_id)?;
            superseded::forget_pass(db, connection, session_id)?;
            turn_index::forget(db, connection, session_id)?;
        }
        Op::CloneEvents { src, dst } => {
            turn_index::replace_revision(db, connection, dst)?;
            db.execute(
                connection,
                "DELETE FROM events WHERE session_id = ?1",
                (dst.as_str(),),
            )?;
            db.execute(
                connection,
                "INSERT INTO events (session_id, position, line) \
                 SELECT ?2, position, line FROM events WHERE session_id = ?1",
                (src.as_str(), dst.as_str()),
            )?;
            activity::recompute(db, connection, dst)?;
            superseded::forget_pass(db, connection, dst)?;
            turn_index::copy(db, connection, src, dst)?;
        }
        Op::UpsertMeta(meta) => {
            let mut previous_parent = None;
            db.query(
                connection,
                "SELECT json_extract(body, '$.parent_session_id') IS NOT NULL FROM sessions WHERE id = ?1",
                [meta.id.as_str()],
                |row| {
                    previous_parent = Some(integer(row, 0)? != 0);
                    Ok(())
                },
            )?;
            let body = serde_json::to_vec(meta.as_ref()).map_err(invalid_data)?;
            db.execute(
                connection,
                "INSERT INTO sessions (id, body) VALUES (?1, ?2) \
                 ON CONFLICT (id) DO UPDATE SET body = excluded.body",
                (meta.id.as_str(), body),
            )?;
            superseded::mark_empty(db, connection, &meta.id)?;
            if previous_parent != Some(meta.parent_session_id.is_some()) {
                activity::recompute(db, connection, &meta.id)?;
            }
        }
        Op::UpsertProject(project) => {
            if let Some(previous) = stored_project(db, connection, &project.id)?
                && previous.icon_path != project.icon_path
            {
                icons.push(previous.icon_path);
            }
            let body = serde_json::to_vec(project.as_ref()).map_err(invalid_data)?;
            db.execute(
                connection,
                "INSERT INTO projects (id, body) VALUES (?1, ?2) \
                 ON CONFLICT (id) DO UPDATE SET body = excluded.body",
                (project.id.as_str(), body),
            )?;
        }
        Op::RemoveSession(id) => {
            turn_index::replace_revision(db, connection, id)?;
            activity::remove(db, connection, id)?;
            db.execute(
                connection,
                "DELETE FROM events WHERE session_id = ?1",
                (id.as_str(),),
            )?;
            db.execute(
                connection,
                "DELETE FROM sessions WHERE id = ?1",
                (id.as_str(),),
            )?;
            superseded::forget_pass(db, connection, id)?;
            turn_index::forget(db, connection, id)?;
            search::forget(db, connection, id)?;
        }
        Op::RemoveProject(id) => {
            if let Some(previous) = stored_project(db, connection, id)? {
                icons.push(previous.icon_path);
            }
            db.execute(
                connection,
                "DELETE FROM projects WHERE id = ?1",
                (id.as_str(),),
            )?;
        }
        Op::KeepWorktree(path) => {
            db.execute(
                connection,
                "INSERT INTO kept_worktrees (path) VALUES (?1) ON CONFLICT (path) DO NOTHING",
                (path.to_string_lossy().as_ref(),),
            )?;
        }
    }
    Ok(())
}

fn stored_project(
    db: &Db,
    connection: &turso::Connection,
    id: &str,
) -> io::Result<Option<Project>> {
    let mut project = None;
    db.query(
        connection,
        "SELECT body FROM projects WHERE id = ?1",
        (id,),
        |row| {
            project = serde_json::from_slice(&blob(row, 0)?).ok();
            Ok(())
        },
    )?;
    Ok(project)
}

fn invalid_data(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

/// Retains the on-disk diff version until the event's historical provider is known.
pub(crate) struct ParsedRecord {
    stored: StoredEvent,
    legacy_file_diff: bool,
}

impl ParsedRecord {
    pub(crate) fn into_stored(mut self, provider: Option<ProviderKind>) -> StoredEvent {
        if self.legacy_file_diff && provider == Some(ProviderKind::Codex) {
            agent::codex::normalize_legacy_file_change_event(&mut self.stored.event);
        }
        self.stored
    }

    pub(crate) fn previous_provider(&self) -> Option<ProviderKind> {
        match self.stored.event {
            AgentEvent::ProviderRelay { from_provider, .. } => Some(from_provider),
            _ => None,
        }
    }
}

pub(crate) fn parse_stored_line(line: &str) -> Result<ParsedRecord, serde_json::Error> {
    let (stored, version) = match serde_json::from_str::<EventEnvelope>(line) {
        Ok(envelope) => (
            StoredEvent {
                origin: envelope.origin,
                author: envelope.author,
                ts: Some(envelope.ts),
                event: envelope.event,
                elided: None,
            },
            envelope.file_diff_version,
        ),
        Err(_) => (
            StoredEvent {
                origin: None,
                author: None,
                ts: None,
                event: serde_json::from_str::<AgentEvent>(line)?,
                elided: None,
            },
            None,
        ),
    };
    let legacy_file_diff = version.is_none()
        && match &stored.event {
            AgentEvent::ItemStarted(item)
            | AgentEvent::ItemUpdated(item)
            | AgentEvent::ItemCompleted(item) => {
                matches!(&item.content, agent::ItemContent::FileChange { changes, .. } if changes.iter().any(|change| matches!(change.kind, agent::FileChangeKind::Create | agent::FileChangeKind::Delete) && change.diff.is_some()))
            }
            AgentEvent::ApprovalRequested(request) => {
                matches!(&request.kind, agent::ApprovalKind::FileChange { changes, .. } if changes.iter().any(|change| matches!(change.kind, agent::FileChangeKind::Create | agent::FileChangeKind::Delete) && change.diff.is_some()))
            }
            _ => false,
        };
    Ok(ParsedRecord {
        stored,
        legacy_file_diff,
    })
}

fn stored_provider_at(
    db: &Db,
    connection: &turso::Connection,
    id: &str,
    position: i64,
) -> io::Result<Option<ProviderKind>> {
    // Metadata names the current provider. The next relay names the provider
    // before it, including when a history page starts in the middle of a turn.
    let mut provider = None;
    let mut start = position;
    loop {
        let mut next = None;
        db.query(
            connection,
            "SELECT position, line FROM events WHERE session_id = ?1 AND position >= ?2 \
             AND instr(line, ?3) > 0 ORDER BY position LIMIT 1",
            (id, start, b"\"provider_relay\"".as_slice()),
            |row| {
                next = Some((integer(row, 0)?, blob(row, 1)?));
                Ok(())
            },
        )?;
        let Some((position, line)) = next else { break };
        if let Ok(Some(record)) = decode_row(&line) {
            provider = record.previous_provider();
            if provider.is_some() {
                break;
            }
        }
        let Some(next) = position.checked_add(1) else {
            break;
        };
        start = next;
    }
    if provider.is_none() {
        db.query(
            connection,
            "SELECT body FROM sessions WHERE id = ?1",
            (id,),
            |row| {
                provider = serde_json::from_slice::<SessionMeta>(&blob(row, 0)?)
                    .ok()
                    .map(|meta| meta.provider);
                Ok(())
            },
        )?;
    }
    Ok(provider)
}

pub use tcode_core::project::now_secs;

/// Current wall-clock time in unix milliseconds (used for event envelopes).
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
