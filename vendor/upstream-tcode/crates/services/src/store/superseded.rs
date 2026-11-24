//! Turn-changes snapshots that a later snapshot supersedes are stored without
//! their diffs. A row keeps its position; only its bytes shrink.
//!
//! Event rows without a session are never served and are left as they are.

use std::io;

use tcode_core::session::{Timeline, TurnSnapshots, drop_turn_diffs};
use turso::Connection;

use super::db::{Db, blob, integer, text};
use super::{EventEnvelopeRef, SessionStore, decode_row, invalid_data};

/// A thread named here has had [`SessionStore::drop_superseded_diffs`] run on
/// it; `undecodable_position` is the row that made it leave the thread as it
/// was, if any. Later appends keep the thread's rows that way themselves.
pub(super) const DIFF_PASS_TABLE: &str = "CREATE TABLE IF NOT EXISTS diff_pass (\
    session_id TEXT PRIMARY KEY, \
    undecodable_position INTEGER\
)";

/// Every row holding a snapshot contains this; a thread without one is left
/// unread.
const SNAPSHOT_TAG: &[u8] = b"\"turn_changes_updated\"";

/// What [`SessionStore::drop_superseded_diffs`] did to one thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffPass {
    /// The diffs of `rows` superseded snapshots were dropped; those rows went
    /// from `before` to `after` bytes.
    Dropped {
        rows: usize,
        before: u64,
        after: u64,
    },
    /// No row held a superseded snapshot's diff.
    Unchanged,
    /// The row at `position` holds no record this build reads, so which
    /// snapshots are superseded is unknown; no row was changed.
    Undecodable { position: u64 },
}

impl SessionStore {
    /// The sessions that [`SessionStore::drop_superseded_diffs`] has not run
    /// on.
    pub fn threads_without_diff_pass(&self) -> io::Result<Vec<String>> {
        self.run("list threads without a diff pass", |db| {
            db.read(|db, connection| {
                let mut ids = Vec::new();
                db.query(
                    connection,
                    "SELECT id FROM sessions \
                     WHERE id NOT IN (SELECT session_id FROM diff_pass)",
                    (),
                    |row| {
                        ids.push(text(row, 0)?);
                        Ok(())
                    },
                )?;
                Ok(ids)
            })
        })
    }

    /// Drop the diffs of every turn-changes snapshot in the thread's log that
    /// a later one supersedes, as the fold of its rows decides, and record that
    /// the thread was dealt with, all in one transaction. Any row that is
    /// neither a record nor blank leaves every row as it is: a tolerant read
    /// would fold without it and could name the wrong snapshots.
    pub fn drop_superseded_diffs(&self, id: &str) -> io::Result<DiffPass> {
        let outcome = self.run("drop superseded diffs", |db| {
            db.write(|db, connection| {
                let outcome = pass(db, connection, id)?;
                let undecodable = match outcome {
                    DiffPass::Undecodable { position } => Some(position as i64),
                    _ => None,
                };
                db.execute(
                    connection,
                    "INSERT INTO diff_pass (session_id, undecodable_position) VALUES (?1, ?2) \
                     ON CONFLICT (session_id) DO UPDATE \
                     SET undecodable_position = excluded.undecodable_position",
                    (id, undecodable),
                )?;
                Ok(outcome)
            })
        })?;
        if matches!(outcome, DiffPass::Dropped { .. }) {
            self.advance_event_generations([id]);
        }
        Ok(outcome)
    }
}

fn pass(db: &Db, connection: &Connection, id: &str) -> io::Result<DiffPass> {
    let mut tagged = false;
    db.query(
        connection,
        "SELECT 1 FROM events WHERE session_id = ?1 AND instr(line, ?2) > 0 LIMIT 1",
        (id, SNAPSHOT_TAG),
        |_| {
            tagged = true;
            Ok(())
        },
    )?;
    if !tagged {
        return Ok(DiffPass::Unchanged);
    }
    let mut fold = Timeline::default();
    let mut snapshots = TurnSnapshots::default();
    let mut superseded = Vec::new();
    let mut undecodable = None;
    db.query(
        connection,
        "SELECT position, line FROM events WHERE session_id = ?1 ORDER BY position",
        (id,),
        |row| {
            if undecodable.is_some() {
                return Ok(());
            }
            let position = integer(row, 0)? as u64;
            match decode_row(&blob(row, 1)?) {
                Ok(Some(record)) => {
                    let stored = record.into_stored(None);
                    superseded.extend(snapshots.apply_at(
                        &mut fold,
                        stored.ts,
                        &stored.event,
                        position,
                    ));
                }
                Ok(None) => {}
                Err(_) => undecodable = Some(position),
            }
            Ok(())
        },
    )?;
    if let Some(position) = undecodable {
        return Ok(DiffPass::Undecodable { position });
    }
    let (mut rows, mut before, mut after) = (0, 0, 0);
    for position in superseded {
        if let Some((old, new)) = rewrite_without_diffs(db, connection, id, position)? {
            rows += 1;
            before += old;
            after += new;
        }
    }
    Ok(if rows == 0 {
        DiffPass::Unchanged
    } else {
        DiffPass::Dropped {
            rows,
            before,
            after,
        }
    })
}

pub(super) fn drop_row_diffs(
    db: &Db,
    connection: &Connection,
    session_id: &str,
    position: u64,
) -> io::Result<()> {
    rewrite_without_diffs(db, connection, session_id, position).map(drop)
}

/// Rewrite the row at `position` without the diffs of the snapshot it holds,
/// when it holds one with diffs. Returns the row's
/// byte length before and after.
fn rewrite_without_diffs(
    db: &Db,
    connection: &Connection,
    session_id: &str,
    position: u64,
) -> io::Result<Option<(u64, u64)>> {
    let mut line = None;
    db.query(
        connection,
        "SELECT line FROM events WHERE session_id = ?1 AND position = ?2",
        (session_id, position as i64),
        |row| {
            line = Some(blob(row, 0)?);
            Ok(())
        },
    )?;
    let Some(line) = line else {
        return Ok(None);
    };
    let Some(rewritten) = without_diffs(&line)? else {
        return Ok(None);
    };
    let lengths = (line.len() as u64, rewritten.len() as u64);
    db.execute(
        connection,
        "UPDATE events SET line = ?3 WHERE session_id = ?1 AND position = ?2",
        (session_id, position as i64, rewritten),
    )?;
    Ok(Some(lengths))
}

/// `line` rewritten without the diffs of the snapshot it holds, in the form it
/// was stored in; `None` when the row holds no snapshot with a diff.
fn without_diffs(line: &[u8]) -> io::Result<Option<Vec<u8>>> {
    let Ok(Some(record)) = decode_row(line) else {
        return Ok(None);
    };
    let mut stored = record.into_stored(None);
    if !drop_turn_diffs(&mut stored.event) {
        return Ok(None);
    }
    let mut rewritten = match stored.ts {
        Some(ts) => serde_json::to_vec(&EventEnvelopeRef::new(
            ts,
            &stored.event,
            stored.author.as_ref(),
            stored.origin,
        )),
        None => serde_json::to_vec(&stored.event),
    }
    .map_err(invalid_data)?;
    if line.ends_with(b"\n") {
        rewritten.push(b'\n');
    }
    if !matches!(decode_row(&rewritten), Ok(Some(read)) if read.stored == stored) {
        return Err(invalid_data(
            "a turn-changes snapshot without its diffs does not read back as itself",
        ));
    }
    Ok(Some(rewritten))
}

/// Forget that the thread's rows were dealt with, for a write that replaced
/// them or left a superseded snapshot's diff behind.
pub(super) fn forget_pass(db: &Db, connection: &Connection, session_id: &str) -> io::Result<()> {
    db.execute(
        connection,
        "DELETE FROM diff_pass WHERE session_id = ?1",
        (session_id,),
    )
    .map(drop)
}

pub(super) fn mark_empty(db: &Db, connection: &Connection, session_id: &str) -> io::Result<()> {
    db.execute(
        connection,
        "INSERT INTO diff_pass (session_id, undecodable_position) \
         SELECT ?1, NULL WHERE NOT EXISTS (SELECT 1 FROM events WHERE session_id = ?1) \
         ON CONFLICT (session_id) DO NOTHING",
        (session_id,),
    )?;
    Ok(())
}
