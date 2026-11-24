use std::{
    borrow::Cow,
    io,
    ops::Range,
    sync::{Arc, Mutex},
};

use serde::Deserialize;
use tcode_core::session::StoredEvent;
use turso::Connection;

use super::{EventLog, SessionStore, TurnIndex, blob, decode_row, integer, invalid_data};

#[derive(Default, Deserialize)]
struct EventAddress {
    event: Option<Box<EventAddress>>,
    #[serde(rename = "type", default)]
    pub event_type: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub item_id: Option<String>,
}

/// A writer-ordered cut. The end, turn index and payload reads share a database
/// snapshot; appends, diff stripping and replacement cannot move that cut.
pub struct LogRead {
    store: SessionStore,
    connection: Arc<Mutex<Option<Connection>>>,
    id: String,
    previews: std::sync::Mutex<Vec<super::projection::Preview>>,
    pub end: u64,
    pub index: Option<TurnIndex>,
}

impl SessionStore {
    pub fn snapshot_log(&self, id: &str) -> io::Result<LogRead> {
        self.run("pin event log", |db| {
            let connection = db.connect_reader()?;
            db.execute(&connection, "BEGIN", ())?;
            let cut = (|| {
                let mut end = 0;
                db.query(
                    &connection,
                    "SELECT max(position) FROM events WHERE session_id = ?1",
                    (id,),
                    |row| {
                        if let Ok(turso::Value::Integer(last)) = row.get_value(0) {
                            end = last as u64 + 1;
                        }
                        Ok(())
                    },
                )?;
                let index = super::turn_index::read(db, &connection, id)?;
                Ok::<_, io::Error>((end, index))
            })();
            let (end, index) = match cut {
                Ok(cut) => cut,
                Err(error) => {
                    let _ = db.execute(&connection, "ROLLBACK", ());
                    return Err(error);
                }
            };
            Ok(LogRead {
                store: self.clone(),
                connection: db.pin_reader(connection),
                id: id.to_owned(),
                previews: std::sync::Mutex::new(Vec::new()),
                end,
                index,
            })
        })
    }
}

impl Drop for LogRead {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.lock().unwrap().take() {
            let _ = futures_lite::future::block_on(connection.execute("ROLLBACK", ()));
        }
    }
}

impl LogRead {
    pub fn read_item(&self, item_id: &str) -> io::Result<Option<EventLog>> {
        let quoted_id = serde_json::to_vec(item_id).map_err(invalid_data)?;
        let id = memchr::memmem::Finder::new(&quoted_id);
        let rewind = memchr::memmem::Finder::new(br#""rewind_completed""#);
        self.store.run("locate item", |db| {
            let guard = self.connection.lock().unwrap();
            let connection = guard.as_ref().ok_or_else(|| io::Error::other("event read cancelled"))?;
            let Some(index) = &self.index else { return Ok(None); };
            let mut turn = None;
            let mut last_rewind = None;
            let mut ambiguous = false;
            let mut log = EventLog { next_row: self.end, ..EventLog::default() };
            db.query(connection,
                "SELECT position, line FROM events WHERE session_id=?1 AND position<?2 ORDER BY position",
                (self.id.as_str(), self.end as i64), |row| {
                    let bytes = blob(row, 1)?;
                    // Escaped JSON addresses can spell the same id differently.
                    if id.find(&bytes).is_none() && rewind.find(&bytes).is_none()
                        && memchr::memchr(b'\\', &bytes).is_none() {
                        return Ok(());
                    }
                    let Ok(envelope) = serde_json::from_slice::<EventAddress>(&bytes) else { return Ok(()); };
                    let address = envelope.event.as_deref().unwrap_or(&envelope);
                    let position = integer(row, 0)? as u64;
                    if address.event_type == "rewind_completed" { last_rewind = Some(position); }
                    if address.id.as_deref() == Some(item_id) || address.item_id.as_deref() == Some(item_id) {
                        let physical_turn = index.starts.partition_point(|start| *start <= position);
                        if turn.is_some_and(|turn| turn != physical_turn) { ambiguous = true; }
                        turn = Some(physical_turn);
                        if let Ok(Some(record)) = decode_row(&bytes) {
                            log.rows.push(position);
                            log.records.push(record.into_stored(None));
                        }
                    }
                    Ok(())
                })?;
            let start = turn.and_then(|turn| turn.checked_sub(1)).map_or(0, |turn| index.starts[turn]);
            // A rewind changes item visibility without mentioning its id.
            if ambiguous || last_rewind.is_some_and(|rewind| rewind >= start) { return Ok(None); }
            Ok(Some(log))
        })
    }

    /// Stops at the consumer's byte budget. Cached projections and canonical
    /// fallbacks are read in the same transaction, in either cursor direction.
    pub fn visit_projected_rows(
        &self,
        positions: Range<u64>,
        backwards: bool,
        version: &str,
        cache_bytes: usize,
        project: impl FnMut(&StoredEvent) -> Cow<'_, StoredEvent>,
        mut visit: impl FnMut(u64, StoredEvent) -> bool,
    ) -> io::Result<()> {
        self.store.run("read event previews", |db| {
            let guard = self.connection.lock().unwrap();
            let connection = guard.as_ref().ok_or_else(|| io::Error::other("event read cancelled"))?;
            let mut project = project;
            let mut previews = Vec::new();
            db.query_while(connection, &format!(
                "SELECT e.position, CASE WHEN p.body IS NOT NULL THEN p.body ELSE e.line END, p.body IS NOT NULL \
                 FROM events e LEFT JOIN event_preview_v1 p ON p.session_id=e.session_id AND p.position=e.position AND p.version=?4 \
                 WHERE e.session_id=?1 AND e.position>=?2 AND e.position<?3 ORDER BY e.position {}",
                if backwards { "DESC" } else { "ASC" }),
                (self.id.as_str(), positions.start.min(self.end) as i64, positions.end.min(self.end) as i64, version), |row| {
                    let position = integer(row, 0)? as u64;
                    let line = blob(row, 1)?;
                    let record = if integer(row, 2)? != 0 {
                        serde_json::from_slice(&line).map_err(invalid_data)?
                    } else {
                        let record = match decode_row(&line) { Ok(Some(record)) => record, _ => return Ok(true) };
                        if record.legacy_file_diff {
                            return Err(io::Error::other("legacy diff needs full hydration"));
                        }
                        let record = record.into_stored(None);
                        match project(&record) {
                            Cow::Borrowed(_) => record,
                            Cow::Owned(preview) => {
                                if line.len() >= cache_bytes && let Ok(body) = serde_json::to_vec(&preview) && body.len() < cache_bytes {
                                    previews.push(super::projection::Preview { position, canonical: line, version: version.to_owned(), body });
                                }
                                preview
                            }
                        }
                    };
                    Ok(visit(position, record))
                })?;
            self.previews.lock().unwrap().extend(previews);
            Ok(())
        })
    }

    pub fn take_publication(&self) -> super::ProjectionPublication {
        super::ProjectionPublication {
            store: self.store.clone(),
            id: self.id.clone(),
            rows: self
                .previews
                .lock()
                .map(|mut rows| std::mem::take(&mut *rows))
                .unwrap_or_default(),
        }
    }
}
