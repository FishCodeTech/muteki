use std::{collections::HashMap, io};

use tcode_core::{
    session::StoredEvent,
    settlement::{THREAD_ACTIVITY_FOLD_VERSION, ThreadActivity},
};
use turso::Connection;

use super::db::{Db, blob, integer, text};
use super::{SessionStore, decode_row, invalid_data};

pub(super) fn create_table(db: &Db, connection: &Connection) -> io::Result<()> {
    for version in 1..=2 {
        for suffix in ["insert", "update", "delete", "remove"] {
            db.execute(
                connection,
                &format!("DROP TRIGGER IF EXISTS activity_cache_v{version}_{suffix}"),
                (),
            )?;
        }
        db.execute(
            connection,
            &format!("DROP TABLE IF EXISTS activity_cache_v{version}"),
            (),
        )?;
    }
    db.execute(
        connection,
        "CREATE TABLE IF NOT EXISTS thread_activity (\
         session_id TEXT PRIMARY KEY, fold_version INTEGER NOT NULL, \
         has_parent INTEGER NOT NULL, body BLOB NOT NULL)",
        (),
    )?;
    Ok(())
}

fn session_parent(db: &Db, connection: &Connection, id: &str) -> io::Result<Option<bool>> {
    let mut parent = None;
    db.query(
        connection,
        "SELECT json_extract(body, '$.parent_session_id') IS NOT NULL FROM sessions WHERE id = ?1",
        [id],
        |row| {
            parent = Some(integer(row, 0)? != 0);
            Ok(())
        },
    )?;
    Ok(parent)
}

pub(super) fn log_end(db: &Db, connection: &Connection, id: &str) -> io::Result<i64> {
    let mut end = 0;
    db.query(
        connection,
        "SELECT COALESCE(MAX(position) + 1, 0) FROM events WHERE session_id = ?1",
        [id],
        |row| {
            end = integer(row, 0)?;
            Ok(())
        },
    )?;
    Ok(end)
}

fn read(
    db: &Db,
    connection: &Connection,
    id: &str,
    parent: bool,
) -> io::Result<Option<ThreadActivity>> {
    let mut activity = None;
    db.query(
        connection,
        "SELECT body FROM thread_activity WHERE session_id = ?1 AND fold_version = ?2 AND has_parent = ?3",
        (id, i64::from(THREAD_ACTIVITY_FOLD_VERSION), i64::from(parent)),
        |row| {
            match serde_json::from_slice(&blob(row, 0)?) {
                Ok(value) => activity = Some(value),
                Err(error) => log::warn!("thread activity for {id} will be rebuilt: {error}"),
            }
            Ok(())
        },
    )?;
    Ok(activity)
}

fn save(
    db: &Db,
    connection: &Connection,
    id: &str,
    parent: bool,
    activity: &ThreadActivity,
) -> io::Result<()> {
    db.execute(
        connection,
        "INSERT INTO thread_activity (session_id, fold_version, has_parent, body) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT (session_id) DO UPDATE SET fold_version = excluded.fold_version, \
         has_parent = excluded.has_parent, body = excluded.body",
        (id, i64::from(THREAD_ACTIVITY_FOLD_VERSION), i64::from(parent), serde_json::to_vec(activity).map_err(invalid_data)?),
    )?;
    Ok(())
}

fn fold(db: &Db, connection: &Connection, id: &str, parent: bool) -> io::Result<ThreadActivity> {
    let mut records = Vec::new();
    db.query(
        connection,
        "SELECT line FROM events WHERE session_id = ?1 ORDER BY position",
        [id],
        |row| {
            if let Ok(Some(record)) = decode_row(&blob(row, 0)?)
                && ThreadActivity::moves_clock(&record.stored.event)
            {
                records.push(record.stored);
            }
            Ok(())
        },
    )?;
    Ok(ThreadActivity::fold_stored(&records, parent))
}

pub(super) fn recompute(db: &Db, connection: &Connection, id: &str) -> io::Result<ThreadActivity> {
    let parent = session_parent(db, connection, id)?.unwrap_or(false);
    let activity = fold(db, connection, id, parent)?;
    save(db, connection, id, parent, &activity)?;
    Ok(activity)
}

pub(super) fn append(
    db: &Db,
    connection: &Connection,
    id: &str,
    record: &StoredEvent,
) -> io::Result<()> {
    let mut stored = None;
    db.query(
        connection,
        "SELECT has_parent, body FROM thread_activity WHERE session_id = ?1 AND fold_version = ?2",
        (id, i64::from(THREAD_ACTIVITY_FOLD_VERSION)),
        |row| {
            if let Ok(activity) = serde_json::from_slice::<ThreadActivity>(&blob(row, 1)?) {
                stored = Some((integer(row, 0)? != 0, activity));
            }
            Ok(())
        },
    )?;
    if let Some((parent, mut activity)) = stored {
        activity.apply(record, parent);
        save(db, connection, id, parent, &activity)?;
    }
    Ok(())
}

pub(super) fn remove(db: &Db, connection: &Connection, id: &str) -> io::Result<()> {
    db.execute(
        connection,
        "DELETE FROM thread_activity WHERE session_id = ?1",
        [id],
    )?;
    Ok(())
}

impl SessionStore {
    pub fn load_thread_activity(&self) -> io::Result<HashMap<String, ThreadActivity>> {
        self.run("load thread activity", |db| {
            db.read(|db, connection| {
                let mut activities = HashMap::new();
                db.query(
                    connection,
                    "SELECT session_id, body FROM thread_activity WHERE fold_version = ?1",
                    [i64::from(THREAD_ACTIVITY_FOLD_VERSION)],
                    |row| {
                        let id = text(row, 0)?;
                        match serde_json::from_slice(&blob(row, 1)?) {
                            Ok(activity) => {
                                activities.insert(id, activity);
                            }
                            Err(error) => {
                                log::warn!("thread activity for {id} will be rebuilt: {error}")
                            }
                        }
                        Ok(())
                    },
                )?;
                Ok(activities)
            })
        })
    }

    /// A `None` activity must be retried because its log changed during the fold.
    /// Sessions absent from the store are omitted from the result.
    pub fn backfill_thread_activity(
        &self,
        ids: &[String],
    ) -> io::Result<HashMap<String, Option<ThreadActivity>>> {
        self.run("backfill thread activity", |db| {
            let mut activities = HashMap::new();
            for id in ids {
                let candidate = db.read(|db, connection| {
                    db.execute(connection, "BEGIN", ())?;
                    let result = (|| {
                        let Some(parent) = session_parent(db, connection, id)? else {
                            return Ok(None);
                        };
                        let end = log_end(db, connection, id)?;
                        let revision = super::turn_index::revision(db, connection, id)?;
                        let activity = match read(db, connection, id, parent)? {
                            Some(activity) => activity,
                            None => fold(db, connection, id, parent)?,
                        };
                        let index = super::turn_index::candidate(db, connection, id)?;
                        Ok(Some((parent, end, revision, activity, index)))
                    })();
                    db.execute(connection, "ROLLBACK", ())?;
                    result
                })?;
                let Some((parent, end, revision, activity, index)) = candidate else {
                    continue;
                };
                let published = db.write(|db, connection| {
                    if session_parent(db, connection, id)? != Some(parent) {
                        return Ok(None);
                    }
                    if log_end(db, connection, id)? != end
                        || super::turn_index::revision(db, connection, id)? != revision
                    {
                        return Ok(None);
                    }
                    if let Some(index) = index
                        && super::turn_index::read(db, connection, id)?.is_none()
                    {
                        super::turn_index::save(db, connection, id, &index)?;
                    }
                    if let Some(current) = read(db, connection, id, parent)? {
                        return Ok(Some(current));
                    }
                    save(db, connection, id, parent, &activity)?;
                    Ok(Some(activity))
                })?;
                activities.insert(id.clone(), published);
            }
            Ok(activities)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Mutation;
    use agent::ProviderKind;
    use tcode_core::project::SessionMeta;

    const LOG: &str = concat!(
        "{\"ts\":100,\"event\":{\"type\":\"item_completed\",\"id\":\"human\",\"content\":{\"kind\":\"user_message\",\"text\":\"request\"}}}\n",
        "{\"ts\":101,\"event\":{\"type\":\"turn_started\",\"turn_id\":\"one\"}}\n",
        "{\"ts\":102,\"origin\":\"human\",\"event\":{\"type\":\"steer_requested\",\"request_id\":\"steer\",\"text\":\"change\"}}\n",
        "{\"ts\":103,\"event\":{\"type\":\"turn_completed\",\"turn_id\":\"one\",\"status\":\"failed\",\"usage\":null}}\n",
        "{\"ts\":104,\"event\":{\"type\":\"turn_started\",\"turn_id\":\"two\"}}\n",
        "{\"ts\":105,\"event\":{\"type\":\"turn_completed\",\"turn_id\":\"two\",\"status\":\"interrupted\",\"usage\":null}}\n",
        "{\"ts\":106,\"origin\":\"agent\",\"event\":{\"type\":\"steer_requested\",\"request_id\":\"agent\",\"text\":\"continue\"}}\n",
        "{\"ts\":107,\"event\":{\"type\":\"provider_start_failed\",\"error\":\"cannot start\"}}\n",
        "{\"ts\":108,\"event\":{\"type\":\"turn_started\",\"turn_id\":\"three\"}}\n",
        "{\"ts\":109,\"event\":{\"type\":\"turn_completed\",\"turn_id\":\"three\",\"status\":\"completed\",\"usage\":null}}\n",
        "{\"ts\":90,\"event\":{\"type\":\"item_completed\",\"id\":\"late\",\"content\":{\"kind\":\"user_message\",\"text\":\"older stamp\"}}}\n",
    );

    #[test]
    fn committed_activity_matches_replay_through_append_import_parent_changes_and_deletion() {
        let root = std::env::temp_dir().join(format!("thread-activity-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::open_at(root.clone()).unwrap();
        let records: Vec<_> = LOG
            .lines()
            .map(|line| super::super::parse_stored_line(line).unwrap().stored)
            .collect();
        for parent in [false, true] {
            let mut meta = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
            meta.parent_session_id = parent.then(|| "lead".into());
            store.upsert_meta(&meta).unwrap();
            for (index, record) in records.iter().enumerate() {
                store
                    .apply(&[Mutation::append_authored_event(
                        &meta.id,
                        record.ts.unwrap(),
                        &record.event,
                        None,
                        record.origin,
                        |_| None,
                    )
                    .unwrap()])
                    .unwrap();
                assert_eq!(
                    store.load_thread_activity().unwrap()[&meta.id],
                    ThreadActivity::fold_stored(&records[..=index], parent)
                );
                if index == 0 {
                    meta.parent_session_id = (!parent).then(|| "lead".into());
                    store.upsert_meta(&meta).unwrap();
                    assert_eq!(
                        store.load_thread_activity().unwrap()[&meta.id],
                        ThreadActivity::fold_stored(&records[..=index], !parent)
                    );
                    meta.parent_session_id = parent.then(|| "lead".into());
                    store.upsert_meta(&meta).unwrap();
                }
            }
            meta.parent_session_id = (!parent).then(|| "lead".into());
            store.upsert_meta(&meta).unwrap();
            assert_eq!(
                store.load_thread_activity().unwrap()[&meta.id],
                ThreadActivity::fold_stored(&records, !parent)
            );
            store
                .apply(&[Mutation::replace_event_log(
                    &meta.id,
                    LOG.lines().next().unwrap().as_bytes().to_vec(),
                )])
                .unwrap();
            assert_eq!(
                store.load_thread_activity().unwrap()[&meta.id],
                ThreadActivity::fold_stored(&records[..1], !parent)
            );
            let mut fork = meta.clone();
            fork.id = uuid::Uuid::new_v4().to_string();
            store
                .apply(&[
                    Mutation::clone_events(&meta.id, &fork.id),
                    Mutation::upsert_meta(fork.clone()),
                ])
                .unwrap();
            assert_eq!(
                store.load_thread_activity().unwrap()[&fork.id],
                ThreadActivity::fold_stored(&records[..1], !parent)
            );
            store.remove_session(&meta.id).unwrap();
            assert!(!store.load_thread_activity().unwrap().contains_key(&meta.id));
            store.remove_session(&fork.id).unwrap();
        }
        store.close().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn activity_backfill_resumes_per_session_and_rebuilds_a_new_fold_version() {
        let root =
            std::env::temp_dir().join(format!("activity-migration-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::open_at(root.clone()).unwrap();
        let mut ids = Vec::new();
        for _ in 0..3 {
            let meta = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
            store.upsert_meta(&meta).unwrap();
            store
                .apply(&[Mutation::replace_event_log(
                    &meta.id,
                    LOG.as_bytes().to_vec(),
                )])
                .unwrap();
            ids.push(meta.id);
        }
        store
            .run("seed old activity version", |db| {
                db.write(|db, connection| {
                    db.execute(
                        connection,
                        "UPDATE thread_activity SET fold_version = 0",
                        (),
                    )?;
                    db.execute(connection, "UPDATE thread_activity SET fold_version = ?2, body = ?3 WHERE session_id = ?1", (ids[2].as_str(), i64::from(THREAD_ACTIVITY_FOLD_VERSION), b"not-json".as_slice()))?;
                    Ok(())
                })
            })
            .unwrap();
        assert!(store.load_thread_activity().unwrap().is_empty());
        store.backfill_thread_activity(&ids[..1]).unwrap();
        store.close().unwrap();
        let store = SessionStore::open_at(root.clone()).unwrap();
        assert_eq!(
            store
                .load_thread_activity()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            vec![&ids[0]]
        );
        store
            .append_event(
                &ids[1],
                200,
                &agent::AgentEvent::ProviderStartFailed {
                    error: "start failed".into(),
                },
            )
            .unwrap();
        assert!(!store.load_thread_activity().unwrap().contains_key(&ids[1]));
        store.backfill_thread_activity(&ids).unwrap();
        let activities = store.load_thread_activity().unwrap();
        for id in ids {
            assert_eq!(
                activities[&id],
                ThreadActivity::fold_stored(&store.read_events(&id).unwrap(), false)
            );
        }
        store.close().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
