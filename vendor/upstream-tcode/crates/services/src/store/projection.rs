use std::io;

use turso::Connection;

use super::SessionStore;
use super::db::Db;

pub(super) fn create_cache(db: &Db, connection: &Connection) -> io::Result<()> {
    db.execute(
        connection,
        "CREATE TABLE IF NOT EXISTS event_preview_v1 (\
         session_id TEXT NOT NULL, position INTEGER NOT NULL, version TEXT NOT NULL, \
         token TEXT NOT NULL, body BLOB, PRIMARY KEY (session_id, position))",
        (),
    )?;
    for (name, event, affected) in [
        (
            "insert",
            "INSERT",
            "session_id = NEW.session_id AND position = NEW.position",
        ),
        (
            "update",
            "UPDATE",
            "(session_id = OLD.session_id AND position = OLD.position) OR (session_id = NEW.session_id AND position = NEW.position)",
        ),
        (
            "delete",
            "DELETE",
            "session_id = OLD.session_id AND position = OLD.position",
        ),
    ] {
        db.execute(
            connection,
            &format!(
                "CREATE TRIGGER IF NOT EXISTS event_preview_v1_{name} AFTER {event} ON events \
             BEGIN DELETE FROM event_preview_v1 WHERE {affected}; END"
            ),
            (),
        )?;
    }
    db.execute(
        connection,
        "CREATE TRIGGER IF NOT EXISTS event_preview_v1_remove AFTER DELETE ON sessions \
         BEGIN DELETE FROM event_preview_v1 WHERE session_id = OLD.id; END",
        (),
    )?;
    Ok(())
}

pub(super) struct Preview {
    pub position: u64,
    pub canonical: Vec<u8>,
    pub version: String,
    pub body: Vec<u8>,
}

/// Publication compares the canonical row before assigning a fresh token.
/// Replacement and diff stripping cannot publish a projection of old bytes.
pub struct ProjectionPublication {
    pub(super) store: SessionStore,
    pub(super) id: String,
    pub(super) rows: Vec<Preview>,
}

impl ProjectionPublication {
    pub fn publish(self) -> io::Result<()> {
        if self.rows.is_empty() {
            return Ok(());
        }
        let token = uuid::Uuid::new_v4().to_string();
        self.store.run("publish event previews", |db| db.write(|db, connection| {
            for row in self.rows {
                db.execute(connection,
                    "INSERT INTO event_preview_v1 (session_id, position, version, token, body) \
                     SELECT ?1, ?2, ?3, ?4, ?5 WHERE EXISTS \
                     (SELECT 1 FROM events WHERE session_id=?1 AND position=?2 AND line=?6) \
                     ON CONFLICT (session_id, position) DO UPDATE SET version=excluded.version, token=excluded.token, body=excluded.body",
                    (self.id.as_str(), row.position as i64, row.version, token.as_str(), row.body, row.canonical))?;
            }
            Ok(())
        }))
    }
}
