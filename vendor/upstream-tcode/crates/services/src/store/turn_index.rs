use std::{collections::HashMap, io};

use serde::{Deserialize, Serialize};
use tcode_core::session::{StoredEvent, TURN_INDEX_FOLD_VERSION, TurnBoundaries, TurnSnapshots};
use turso::Connection;

use super::db::{Db, blob, integer, text};
use super::{SessionStore, decode_row, invalid_data};

pub(super) fn create_tables(db: &Db, connection: &Connection) -> io::Result<()> {
    db.execute(connection, "DROP TABLE IF EXISTS turn_index_v2", ())?;
    db.execute(connection, "DROP TABLE IF EXISTS turn_index_entries_v2", ())?;
    db.execute(connection, "CREATE TABLE IF NOT EXISTS event_log_revisions (session_id TEXT PRIMARY KEY, revision BLOB NOT NULL)", ())?;
    let mut versioned = false;
    db.query(connection, "PRAGMA table_info(turn_index)", (), |row| {
        versioned |= text(row, 1)? == "fold_version";
        Ok(())
    })?;
    if !versioned {
        db.execute(connection, "DROP TABLE IF EXISTS turn_index", ())?;
    }
    db.execute(
        connection,
        "CREATE TABLE IF NOT EXISTS turn_index (\
        session_id TEXT PRIMARY KEY, fold_version INTEGER NOT NULL, \
        turns INTEGER NOT NULL, starts BLOB NOT NULL, state BLOB NOT NULL)",
        (),
    )?;
    Ok(())
}

/// Turn count after replay, and every physical row that opened a turn.
/// Rewinds remove turns but retain the starts of their ended lifetimes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnIndex {
    pub turns: u64,
    pub starts: Vec<u64>,
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    boundaries: TurnBoundaries,
    snapshots: TurnSnapshots<u64>,
    undecodable: bool,
}

#[derive(Default)]
pub(super) struct Pending {
    pub index: TurnIndex,
    state: State,
    dirty: bool,
}

impl SessionStore {
    pub fn turn_index(&self, id: &str) -> io::Result<Option<TurnIndex>> {
        self.run("read a turn index", |db| {
            db.read(|db, connection| read(db, connection, id))
        })
    }

    pub fn threads_without_turn_index(&self) -> io::Result<Vec<String>> {
        self.run("list threads without a turn index", |db| {
            db.read(|db, connection| {
                let mut ids = Vec::new();
                db.query(
                    connection,
                    "SELECT id FROM sessions WHERE id NOT IN \
                    (SELECT session_id FROM turn_index WHERE fold_version = ?1)",
                    [i64::from(TURN_INDEX_FOLD_VERSION)],
                    |row| {
                        ids.push(text(row, 0)?);
                        Ok(())
                    },
                )?;
                Ok(ids)
            })
        })
    }
}

pub(super) fn read(db: &Db, connection: &Connection, id: &str) -> io::Result<Option<TurnIndex>> {
    let mut index = None;
    db.query(
        connection,
        "SELECT turns, starts FROM turn_index WHERE session_id = ?1 AND fold_version = ?2",
        (id, i64::from(TURN_INDEX_FOLD_VERSION)),
        |row| {
            index = Some(TurnIndex {
                turns: integer(row, 0)? as u64,
                starts: serde_json::from_slice(&blob(row, 1)?).map_err(invalid_data)?,
            });
            Ok(())
        },
    )?;
    Ok(index)
}

fn load(db: &Db, connection: &Connection, id: &str) -> io::Result<Option<Pending>> {
    let mut pending = None;
    db.query(
        connection,
        "SELECT turns, starts, state FROM turn_index WHERE session_id = ?1 AND fold_version = ?2",
        (id, i64::from(TURN_INDEX_FOLD_VERSION)),
        |row| {
            pending = Some(Pending {
                index: TurnIndex {
                    turns: integer(row, 0)? as u64,
                    starts: serde_json::from_slice(&blob(row, 1)?).map_err(invalid_data)?,
                },
                state: serde_json::from_slice(&blob(row, 2)?).map_err(invalid_data)?,
                dirty: false,
            });
            Ok(())
        },
    )?;
    Ok(pending)
}

pub(super) fn save(
    db: &Db,
    connection: &Connection,
    id: &str,
    pending: &Pending,
) -> io::Result<()> {
    db.execute(connection, "INSERT INTO turn_index (session_id, fold_version, turns, starts, state) \
        VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (session_id) DO UPDATE SET \
        fold_version = excluded.fold_version, turns = excluded.turns, starts = excluded.starts, state = excluded.state",
        (id, i64::from(TURN_INDEX_FOLD_VERSION), pending.index.turns as i64,
         serde_json::to_vec(&pending.index.starts).map_err(invalid_data)?,
         serde_json::to_vec(&pending.state).map_err(invalid_data)?))?;
    Ok(())
}

impl Pending {
    fn apply(&mut self, position: u64, record: &StoredEvent) -> Option<u64> {
        let (change, superseded) = self.state.snapshots.apply_boundaries(
            &mut self.state.boundaries,
            &record.event,
            position,
        );
        if change.opened {
            self.index.starts.push(position);
        }
        self.index.turns = self.state.boundaries.len() as u64;
        self.dirty |= change.changed;
        superseded.filter(|_| !self.state.undecodable)
    }
}

/// Called under the backfill's read snapshot, never under the writer lock.
pub(super) fn candidate(db: &Db, connection: &Connection, id: &str) -> io::Result<Option<Pending>> {
    if read(db, connection, id)?.is_some() {
        return Ok(None);
    }
    let mut pending = Pending::default();
    db.query(
        connection,
        "SELECT position, line FROM events WHERE session_id = ?1 ORDER BY position",
        [id],
        |row| {
            match decode_row(&blob(row, 1)?) {
                Ok(Some(record)) => {
                    pending.apply(integer(row, 0)? as u64, &record.stored);
                }
                Ok(None) => {}
                Err(_) => pending.state.undecodable = true,
            }
            Ok(())
        },
    )?;
    Ok(Some(pending))
}

pub(super) type Batch = HashMap<String, Option<Pending>>;

pub(super) fn append(
    db: &Db,
    connection: &Connection,
    batch: &mut Batch,
    id: &str,
    position: u64,
    record: &StoredEvent,
) -> io::Result<()> {
    if !batch.contains_key(id) {
        let pending = load(db, connection, id)?.or_else(|| (position == 0).then(Pending::default));
        batch.insert(id.to_owned(), pending);
    }
    if let Some(pending) = batch.get_mut(id).and_then(Option::as_mut) {
        if let Some(superseded) = pending.apply(position, record) {
            super::superseded::drop_row_diffs(db, connection, id, superseded)?;
        }
        if position == 0 {
            pending.dirty = true;
        }
    } else if matches!(record.event, agent::AgentEvent::TurnChangesUpdated { .. }) {
        super::superseded::forget_pass(db, connection, id)?;
    }
    Ok(())
}

pub(super) fn flush(db: &Db, connection: &Connection, batch: &Batch) -> io::Result<()> {
    for (id, pending) in batch {
        if let Some(pending) = pending
            && pending.dirty
        {
            save(db, connection, id, pending)?;
        }
    }
    Ok(())
}

pub(super) fn copy(db: &Db, connection: &Connection, src: &str, dst: &str) -> io::Result<()> {
    forget(db, connection, dst)?;
    db.execute(
        connection,
        "INSERT INTO turn_index (session_id, fold_version, turns, starts, state) \
        SELECT ?2, fold_version, turns, starts, state FROM turn_index WHERE session_id = ?1",
        (src, dst),
    )?;
    Ok(())
}

/// Replacement can keep the same row count; its identity must also match a
/// backfill snapshot before publishing. Appends are detected by their end row.
pub(super) fn revision(db: &Db, connection: &Connection, id: &str) -> io::Result<Vec<u8>> {
    let mut revision = Vec::new();
    db.query(
        connection,
        "SELECT revision FROM event_log_revisions WHERE session_id = ?1",
        [id],
        |row| {
            revision = blob(row, 0)?;
            Ok(())
        },
    )?;
    Ok(revision)
}

pub(super) fn replace_revision(db: &Db, connection: &Connection, id: &str) -> io::Result<()> {
    db.execute(connection, "INSERT INTO event_log_revisions (session_id, revision) VALUES (?1, randomblob(16)) ON CONFLICT (session_id) DO UPDATE SET revision = excluded.revision", [id])?;
    Ok(())
}

pub(super) fn forget(db: &Db, connection: &Connection, id: &str) -> io::Result<()> {
    db.execute(
        connection,
        "DELETE FROM turn_index WHERE session_id = ?1",
        [id],
    )?;
    Ok(())
}
