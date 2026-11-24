//! The text global search reads: each thread's searchable entries, extracted
//! from its folded log and kept with the identity of the log they came from,
//! so a search reads text instead of reading and folding every log.

use std::io;

use turso::Connection;

use super::SessionStore;
use super::db::{Db, blob, integer, text};

pub(super) fn create_tables(db: &Db, connection: &Connection) -> io::Result<()> {
    db.execute(
        connection,
        "CREATE TABLE IF NOT EXISTS search_sources (\
         session_id TEXT PRIMARY KEY, version INTEGER NOT NULL, \
         revision BLOB NOT NULL, end_row INTEGER NOT NULL)",
        (),
    )?;
    // Entries are derived from the logs, so a table an older build created
    // without the position and kind columns is dropped and extracted again.
    let mut positioned = false;
    db.query(connection, "PRAGMA table_info(search_entries)", (), |row| {
        positioned |= text(row, 1)? == "position";
        Ok(())
    })?;
    if !positioned {
        db.execute(connection, "DROP TABLE IF EXISTS search_entries", ())?;
        db.execute(connection, "DELETE FROM search_sources", ())?;
    }
    db.execute(
        connection,
        "CREATE TABLE IF NOT EXISTS search_entries (\
         session_id TEXT NOT NULL, ordinal INTEGER NOT NULL, entry_id TEXT NOT NULL, \
         position INTEGER NOT NULL, turn INTEGER NOT NULL, kind TEXT NOT NULL, \
         text TEXT NOT NULL, folded TEXT NOT NULL, \
         PRIMARY KEY (session_id, ordinal))",
        (),
    )?;
    Ok(())
}

pub(super) fn forget(db: &Db, connection: &Connection, id: &str) -> io::Result<()> {
    db.execute(
        connection,
        "DELETE FROM search_entries WHERE session_id = ?1",
        [id],
    )?;
    db.execute(
        connection,
        "DELETE FROM search_sources WHERE session_id = ?1",
        [id],
    )?;
    Ok(())
}

/// The identity of a thread's log: a replacement changes its revision, an
/// append its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchSource {
    revision: Vec<u8>,
    /// The row after the last one the entries were extracted from.
    pub end: u64,
}

/// One searchable entry as it is kept: `folded` is the lowercased text a
/// query is matched against, `text` what a hit's snippet is cut from, and
/// `position` the log row that first recorded the entry.
pub struct IndexedEntry<'a> {
    pub entry_id: &'a str,
    pub position: u64,
    pub turn: usize,
    pub kind: &'a str,
    pub text: &'a str,
    pub folded: &'a str,
}

/// One stored entry a query matched.
pub struct SearchMatch<'a> {
    pub entry_id: &'a str,
    pub position: u64,
    pub turn: usize,
    pub kind: &'a str,
    pub text: &'a str,
}

fn current_source(db: &Db, connection: &Connection, id: &str) -> io::Result<SearchSource> {
    Ok(SearchSource {
        revision: super::turn_index::revision(db, connection, id)?,
        end: super::activity::log_end(db, connection, id)? as u64,
    })
}

impl SessionStore {
    /// The identity of `id`'s log as it is now.
    pub fn search_source(&self, id: &str) -> io::Result<SearchSource> {
        self.run("read a search source", |db| {
            db.read(|db, connection| current_source(db, connection, id))
        })
    }

    /// Whether `id`'s searchable entries were extracted under `version` from
    /// its log as it is now.
    pub fn search_entries_current(&self, id: &str, version: u32) -> io::Result<bool> {
        self.run("check search entries", |db| {
            db.read(|db, connection| {
                let mut indexed = None;
                db.query(
                    connection,
                    "SELECT revision, end_row FROM search_sources \
                     WHERE session_id = ?1 AND version = ?2",
                    (id, i64::from(version)),
                    |row| {
                        indexed = Some(SearchSource {
                            revision: blob(row, 0)?,
                            end: integer(row, 1)? as u64,
                        });
                        Ok(())
                    },
                )?;
                Ok(indexed.is_some_and(|indexed| {
                    current_source(db, connection, id).is_ok_and(|current| current == indexed)
                }))
            })
        })
    }

    /// Replace `id`'s searchable entries with those extracted under `version`
    /// from the log `source` names.
    pub fn replace_search_entries<'a>(
        &self,
        id: &str,
        version: u32,
        source: &SearchSource,
        entries: impl IntoIterator<Item = IndexedEntry<'a>>,
    ) -> io::Result<()> {
        self.run("write search entries", |db| {
            db.write(|db, connection| {
                forget(db, connection, id)?;
                for (ordinal, entry) in entries.into_iter().enumerate() {
                    db.execute(
                        connection,
                        "INSERT INTO search_entries \
                         (session_id, ordinal, entry_id, position, turn, kind, text, folded) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        (
                            id,
                            ordinal as i64,
                            entry.entry_id,
                            entry.position as i64,
                            entry.turn as i64,
                            entry.kind,
                            entry.text,
                            entry.folded,
                        ),
                    )?;
                }
                db.execute(
                    connection,
                    "INSERT INTO search_sources (session_id, version, revision, end_row) \
                     VALUES (?1, ?2, ?3, ?4)",
                    (
                        id,
                        i64::from(version),
                        source.revision.clone(),
                        source.end as i64,
                    ),
                )?;
                Ok(())
            })
        })
    }

    /// Visit `id`'s searchable entries whose folded text contains `folded`, in
    /// order, while `each` asks for more.
    pub fn visit_search_matches(
        &self,
        id: &str,
        folded: &str,
        mut each: impl FnMut(SearchMatch<'_>) -> bool,
    ) -> io::Result<()> {
        self.run("search entries", |db| {
            db.read(|db, connection| {
                db.query_while(
                    connection,
                    "SELECT entry_id, position, turn, kind, text FROM search_entries \
                     WHERE session_id = ?1 AND instr(folded, ?2) > 0 ORDER BY ordinal",
                    (id, folded),
                    |row| {
                        Ok(each(SearchMatch {
                            entry_id: &text(row, 0)?,
                            position: integer(row, 1)? as u64,
                            turn: integer(row, 2)? as usize,
                            kind: &text(row, 3)?,
                            text: &text(row, 4)?,
                        }))
                    },
                )
            })
        })
    }
}
