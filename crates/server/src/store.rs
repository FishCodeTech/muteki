use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    path::Path,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
    snapshot_connection: Option<Arc<Mutex<Connection>>>,
    document_gate: Arc<parking_lot::ReentrantMutex<()>>,
    post_commit:Arc<Mutex<Vec<Vec<Box<dyn FnOnce()+Send>>>>>,
}

impl Store {
    /// A synchronous domain scope; nested document batches use SQLite savepoints.
    /// The reentrant gate prevents another thread sharing this connection from
    /// participating in the scope, while BEGIN IMMEDIATE fences other processes.
    pub(crate) fn document_scope<T,E>(&self,apply:impl FnOnce()->Result<T,E>)->rusqlite::Result<Result<T,E>>{
        let _gate=self.document_gate.lock();
        let name=format!("src_{}",uuid::Uuid::new_v4().simple());
        let outer={let conn=self.connection.lock().map_err(|_|rusqlite::Error::InvalidQuery)?;let outer=conn.is_autocommit();if outer{conn.execute_batch("BEGIN IMMEDIATE")?;}else{conn.execute_batch(&format!("SAVEPOINT {name}"))?;}outer};
        self.post_commit.lock().map_err(|_|rusqlite::Error::InvalidQuery)?.push(Vec::new());
        let mut rollback=DocumentRollback{store:self,name,outer,armed:true};
        let result=apply();
        {let conn=self.connection.lock().map_err(|_|rusqlite::Error::InvalidQuery)?;
            if result.is_ok(){conn.execute_batch(&if outer{"COMMIT".into()}else{format!("RELEASE {}",rollback.name)})?;}
            else if outer{conn.execute_batch("ROLLBACK")?;}else{conn.execute_batch(&format!("ROLLBACK TO {}; RELEASE {}",rollback.name,rollback.name))?;}
        }
        rollback.armed=false;
        let actions={let mut batches=self.post_commit.lock().map_err(|_|rusqlite::Error::InvalidQuery)?;let actions=batches.pop().unwrap_or_default();if result.is_ok(){if let Some(parent)=batches.last_mut(){parent.extend(actions);Vec::new()}else{actions}}else{Vec::new()}};
        for action in actions{action();}Ok(result)
    }
    pub(crate) fn after_commit(&self,action:impl FnOnce()+Send+'static){
        let _gate=self.document_gate.lock();let mut batches=self.post_commit.lock().unwrap();if let Some(batch)=batches.last_mut(){batch.push(Box::new(action));}else{drop(batches);action();}
    }
    /// Save the transcript and its newly staged events in one transaction.
    pub fn chat_commit(
        &self,
        id: &str,
        value: &Value,
    ) -> rusqlite::Result<(Vec<Value>, u64)> {
        self.chat_commit_checked(id, value, None, None)
            .map(|committed| committed.expect("unconditional chat commit"))
    }

    /// Compare the thread document and optional CommandAPI stream version in
    /// the same SQLite transaction that inserts its events and new document.
    pub(crate) fn chat_commit_checked(
        &self,
        id: &str,
        value: &Value,
        expected_document: Option<&Value>,
        expected_version: Option<u64>,
    ) -> rusqlite::Result<Option<(Vec<Value>, u64)>> {
        self.chat_commit_with_documents(id, value, expected_document, expected_version, &[], &[])
    }

    /// Commit a newly created conversation with its owning domain documents.
    /// Both the conversation event clock and the domain records advance together.
    pub(crate) fn chat_commit_with_documents(
        &self,
        id: &str,
        value: &Value,
        expected_document: Option<&Value>,
        expected_version: Option<u64>,
        expected_records: &[(&str, &str, Option<&Value>)],
        records: &[(&str, &str, &Value)],
    ) -> rusqlite::Result<Option<(Vec<Value>, u64)>> {
        let _connection_gate=self.document_gate.lock();
        let mut conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let tx = document_transaction(&mut conn,rusqlite::TransactionBehavior::Immediate)?;
        if expected_document.is_some() || expected_version.is_some() {
            let current = get_locked(&tx, "conversation_threads", id)?;
            if expected_document.is_some_and(|expected| current.as_ref() != Some(expected)) {
                return Ok(None);
            }
            if let Some(expected) = expected_version {
                let stream = current.as_ref().map(crate::chat::stream_head).unwrap_or(0);
                let key = format!("thread\0{id}");
                let recorded = get_locked(&tx, "platform_aggregate_versions", &key)?
                    .and_then(|row| row["version"].as_u64())
                    .unwrap_or(0);
                if stream.max(recorded) != expected {
                    return Ok(None);
                }
            }
        }
        for (collection, record_id, expected) in expected_records {
            if get_locked(&tx, collection, record_id)?.as_ref() != *expected {
                return Ok(None);
            }
        }
        let initialized: bool =
            tx.query_row("SELECT EXISTS(SELECT 1 FROM chat_event_clock)", [], |r| {
                r.get(0)
            })?;
        if !initialized {
            let mut floor = 0u64;
            if let Some(root) = get_locked(&tx, "system", "history_root")?
                .and_then(|v| v.as_str().map(str::to_owned))
            {
                let path = std::path::Path::new(&root).join("_platform/platform.db");
                let source =
                    Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                floor = source.query_row(
                    "SELECT COALESCE(MAX(seq),0) FROM domain_events",
                    [],
                    |r| r.get(0),
                )?;
            }
            tx.execute(
                "INSERT INTO chat_event_clock(id,head) VALUES(1,?1)",
                [floor],
            )?;
        }
        let mut committed = Vec::new();
        for event in value["_pending_events"].as_array().into_iter().flatten() {
            tx.execute("UPDATE chat_event_clock SET head=head+1 WHERE id=1", [])?;
            let seq: u64 =
                tx.query_row("SELECT head FROM chat_event_clock WHERE id=1", [], |r| {
                    r.get(0)
                })?;
            let mut event = event.clone();
            event["seq"] = json!(seq);
            tx.execute("INSERT INTO chat_events(seq,thread_id,turn_id,stream_seq,event_id,event_type,payload) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![seq,id,event["payload"]["turn_id"].as_str().unwrap_or(""),event["stream_seq"].as_u64(),event["event_id"].as_str(),event["event_type"].as_str(),event.to_string()])?;
            committed.push(event);
        }
        let mut document = value.clone();
        document
            .as_object_mut()
            .ok_or(rusqlite::Error::InvalidQuery)?
            .remove("_pending_events");
        let watermark = tx.query_row("SELECT head FROM chat_event_clock WHERE id=1", [], |r| {
            r.get::<_, u64>(0)
        })?;
        document["event_watermark"] = json!(watermark);
        put_locked(&tx, "conversation_threads", id, &document)?;
        for (collection, record_id, record) in records {
            put_locked(&tx, collection, record_id, record)?;
        }
        tx.commit()?;
        Ok(Some((committed, watermark)))
    }

    /// Commit a child Thread and its parent subagent node together. Both
    /// streams receive durable events before either document becomes visible.
    pub(crate) fn chat_commit_subagent_spawn(&self,parent_id:&str,parent:&Value,expected_parent:&Value,expected_version:Option<u64>,child_id:&str,child:&Value,command_key:&str,command_record:&Value)->rusqlite::Result<Option<(Vec<Value>,Vec<Value>,u64)>>{
        self.chat_commit_many(&[(child_id,child,None),(parent_id,parent,Some(expected_parent))],expected_version.map(|version|(parent_id,version)),&[("conversation_commands",command_key,None)],&[("conversation_commands",command_key,command_record)])
            .map(|result|result.map(|(mut events,watermark)|{let parent=events.pop().unwrap();let child=events.pop().unwrap();(parent,child,watermark)}))
    }
    /// Commit related conversation documents, their event clocks and command records together.
    pub(crate) fn chat_commit_many(&self,documents:&[(&str,&Value,Option<&Value>)],expected_version:Option<(&str,u64)>,expected_records:&[(&str,&str,Option<&Value>)],records:&[(&str,&str,&Value)])->rusqlite::Result<Option<(Vec<Vec<Value>>,u64)>>{
        let _connection_gate=self.document_gate.lock();
        let mut conn=self.connection.lock().map_err(|_|rusqlite::Error::InvalidQuery)?;
        let tx=document_transaction(&mut conn,rusqlite::TransactionBehavior::Immediate)?;
        for(id,_,expected)in documents{if get_locked(&tx,"conversation_threads",id)?.as_ref()!=*expected{return Ok(None);}}
        for(collection,id,expected)in expected_records{if get_locked(&tx,collection,id)?.as_ref()!=*expected{return Ok(None);}}
        if let Some((id,version))=expected_version{
            let current=get_locked(&tx,"conversation_threads",id)?.ok_or(rusqlite::Error::InvalidQuery)?;
            let recorded=get_locked(&tx,"platform_aggregate_versions",&format!("thread\0{id}"))?.and_then(|row|row["version"].as_u64()).unwrap_or(0);
            if crate::chat::stream_head(&current).max(recorded)!=version{return Ok(None);}
        }
        let initialized: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM chat_event_clock)", [], |row| row.get(0))?;
        if !initialized {
            let mut floor = 0u64;
            if let Some(root) = get_locked(&tx, "system", "history_root")?
                .and_then(|value| value.as_str().map(str::to_owned))
            {
                let source = Connection::open_with_flags(
                    Path::new(&root).join("_platform/platform.db"),
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )?;
                floor = source.query_row("SELECT COALESCE(MAX(seq),0) FROM domain_events", [], |row| row.get(0))?;
            }
            tx.execute("INSERT INTO chat_event_clock(id,head) VALUES(1,?1)", [floor])?;
        }
        let mut events=vec![Vec::new();documents.len()];
        for (index,(id,value,_)) in documents.iter().enumerate() {
            for event in value["_pending_events"].as_array().into_iter().flatten() {
                tx.execute("UPDATE chat_event_clock SET head=head+1 WHERE id=1", [])?;
                let seq: u64 = tx.query_row("SELECT head FROM chat_event_clock WHERE id=1", [], |row| row.get(0))?;
                let mut event = event.clone();
                event["seq"] = json!(seq);
                tx.execute("INSERT INTO chat_events(seq,thread_id,turn_id,stream_seq,event_id,event_type,payload) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![seq,id,event["payload"]["turn_id"].as_str().unwrap_or(""),event["stream_seq"].as_u64(),event["event_id"].as_str(),event["event_type"].as_str(),event.to_string()])?;
                events[index].push(event);
            }
        }
        let watermark: u64 = tx.query_row("SELECT head FROM chat_event_clock WHERE id=1", [], |row| row.get(0))?;
        for (id,value,_) in documents {
            let mut document = (*value).clone();
            document.as_object_mut().ok_or(rusqlite::Error::InvalidQuery)?.remove("_pending_events");
            document["event_watermark"] = json!(watermark);
            put_locked(&tx, "conversation_threads", id, &document)?;
        }
        for(collection,id,value)in records{put_locked(&tx,collection,id,value)?;}
        tx.commit()?;Ok(Some((events,watermark)))
    }

    pub(crate) fn chat_events(
        &self,
        tid: &str,
        turn: Option<&str>,
        after: u64,
        through: u64,
        limit: usize,
    ) -> rusqlite::Result<Vec<Value>> {
        let _connection_gate=self.document_gate.lock();
        let conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let sql = if turn.is_some() {
            "SELECT payload FROM chat_events WHERE thread_id=?1 AND turn_id=?2 AND stream_seq>?3 AND stream_seq<=?4 ORDER BY stream_seq LIMIT ?5"
        } else {
            "SELECT payload FROM chat_events WHERE thread_id=?1 AND (?2 IS NULL) AND seq>?3 AND seq<=?4 ORDER BY seq LIMIT ?5"
        };
        let mut stmt = conn.prepare(sql)?;
        stmt.query_map(params![tid, turn, after, through, limit], |r| {
            decode(r.get(0)?)
        })?
        .collect()
    }

    pub(crate) fn chat_tail_cursor(
        &self,
        tid: &str,
        tail: u64,
        running: &str,
    ) -> rusqlite::Result<u64> {
        let _connection_gate=self.document_gate.lock();
        let conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let head: u64 = conn.query_row(
            "SELECT COALESCE(MAX(stream_seq),0) FROM chat_events WHERE thread_id=?1",
            [tid],
            |r| r.get(0),
        )?;
        let start: Option<u64> = conn.query_row("SELECT MIN(seq) FROM chat_events WHERE thread_id=?1 AND (stream_seq>?2 OR (turn_id=?3 AND ?3<>''))", params![tid,head.saturating_sub(tail),running], |r| r.get(0))?;
        Ok(start.unwrap_or(1).saturating_sub(1))
    }

    pub(crate) fn chat_events_mentioning(
        &self,
        thread_id: &str,
        needle: &str,
        alternate: &str,
        limit: usize,
    ) -> rusqlite::Result<Vec<Value>> {
        self.read_snapshot(|connection| {
            let mut statement = connection.prepare(
                "SELECT payload FROM chat_events WHERE thread_id=?1
                 AND event_type IN ('core.tool.completed','core.agent.event')
                 AND (instr(payload,?2)>0 OR instr(payload,?3)>0)
                 ORDER BY seq DESC LIMIT ?4",
            )?;
            statement
                .query_map(params![thread_id, needle, alternate, limit], |row| {
                    decode(row.get(0)?)
                })?
                .collect()
        })
    }

    pub(crate) fn chat_events_before(
        &self,
        thread_id: &str,
        turn_id: &str,
        through: u64,
        limit: usize,
    ) -> rusqlite::Result<Vec<Value>> {
        self.read_snapshot(|connection| {
            let mut statement = connection.prepare(
                "SELECT payload FROM chat_events WHERE thread_id=?1 AND turn_id=?2 AND stream_seq<=?3 ORDER BY stream_seq DESC LIMIT ?4",
            )?;
            statement.query_map(params![thread_id, turn_id, through, limit], |row| {
                decode(row.get(0)?)
            })?.collect()
        })
    }

    /// Workspace metadata and its command receipts form a single durable result.
    pub(crate) fn workspace_commit(
        &self,
        id: &str,
        workspace: Option<&Value>,
        commands: &[(String, Value)],
    ) -> rusqlite::Result<()> {
        let _connection_gate=self.document_gate.lock();
        let mut conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let tx = document_transaction(&mut conn,rusqlite::TransactionBehavior::Immediate)?;
        match workspace {
            Some(value) => put_locked(&tx, "workspaces", id, value)?,
            None => {
                tx.execute(
                    "DELETE FROM documents WHERE collection='workspaces' AND id=?1",
                    [id],
                )?;
            }
        }
        for (key, value) in commands {
            put_locked(&tx, "workspace_commands", key, value)?;
        }
        tx.commit()
    }
    /// A provider identity and its visible transcript are committed together.
    /// The caller holds the conversation mutation lock while importing.
    pub fn import_provider_history(
        &self,
        identity_key: &str,
        identity: &Value,
        mut document: Value,
    ) -> rusqlite::Result<Result<(Value, bool), String>> {
        let _connection_gate=self.document_gate.lock();
        let mut conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let tx = document_transaction(&mut conn,rusqlite::TransactionBehavior::Immediate)?;
        let previous = get_locked(&tx, "provider_history_identities", identity_key)?;
        let legacy_id = previous
            .as_ref()
            .and_then(|v| v["legacy_thread_id"].as_str())
            .or_else(|| {
                if previous.is_none() {
                    identity["legacy_thread_id"].as_str()
                } else {
                    None
                }
            });
        let existing_id = previous
            .as_ref()
            .and_then(|v| v["thread_id"].as_str())
            .or(legacy_id);
        let existing = match existing_id {
            Some(id) => get_locked(&tx, "conversation_threads", id)?,
            None => None,
        };
        let legacy_mode = legacy_id.is_some() && existing.is_some();
        let mut legacy_count = previous
            .as_ref()
            .and_then(|v| v["legacy_message_count"].as_u64())
            .map(|v| v as usize);
        let mut skipped = false;
        if let Some(mut old) = existing {
            let incoming = document["messages"]
                .as_array()
                .ok_or(rusqlite::Error::InvalidQuery)?;
            let rows = old["messages"]
                .as_array()
                .ok_or(rusqlite::Error::InvalidQuery)?;
            let history: Vec<_> = if legacy_mode {
                let count = legacy_count.unwrap_or_else(|| {
                    let imported = rows
                        .iter()
                        .take_while(|row| row["stream_seq"].as_i64().is_some_and(|seq| seq < 0))
                        .count();
                    if imported > 0 { imported } else { rows.len() }
                });
                rows.iter().take(count).collect()
            } else {
                rows.iter()
                    .filter(|m| m["provider_history_identity"] == identity_key)
                    .collect()
            };
            for (a, b) in history.iter().zip(incoming) {
                let mismatch = if legacy_mode {
                    a["role"] != b["role"] || a["text"] != b["text"]
                } else {
                    a["source_role"] != b["source_role"]
                        || a["source_content"] != b["source_content"]
                        || a["text"] != b["text"]
                };
                if mismatch {
                    return Ok(Err(
                        "conversation.import.source_conflict: 已有导入内容与当前来源不同".into(),
                    ));
                }
            }
            skipped = history.len() >= incoming.len();
            if legacy_mode {
                legacy_count = Some(if skipped {
                    legacy_count.unwrap_or(history.len())
                } else {
                    incoming.len()
                });
            }
            if !skipped {
                let mut messages = Vec::with_capacity(rows.len() + incoming.len() - history.len());
                let tid = old["thread"]["thread_id"].clone();
                for (index, row) in incoming.iter().enumerate() {
                    let mut row = row.clone();
                    row["thread_id"] = tid.clone();
                    if let Some(previous) = history.get(index) {
                        row["message_id"] = previous["message_id"].clone();
                    }
                    messages.push(row);
                }
                if legacy_mode {
                    messages.extend(rows.iter().skip(history.len()).cloned());
                } else {
                    messages.extend(
                        rows.iter()
                            .filter(|m| m["provider_history_identity"] != identity_key)
                            .cloned(),
                    );
                }
                old["messages"] = json!(messages);
                old["thread"]["updated_at"] = document["thread"]["updated_at"].clone();
                old["thread"]["state"]["last_message_preview"] =
                    document["thread"]["state"]["last_message_preview"].clone();
            }
            document = old;
        }
        let tid = document["thread"]["thread_id"]
            .as_str()
            .ok_or(rusqlite::Error::InvalidQuery)?;
        let mut identity = identity.clone();
        identity["thread_id"] = json!(tid);
        if legacy_mode {
            identity["legacy_thread_id"] = json!(tid);
            identity["legacy_message_count"] = json!(legacy_count);
        }
        if !skipped {
            put_locked(&tx, "conversation_threads", tid, &document)?;
        }
        put_locked(&tx, "provider_history_identities", identity_key, &identity)?;
        tx.commit()?;
        Ok(Ok((document, skipped)))
    }
    pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let _init_lock = if path.as_ref() != Path::new(":memory:") {
            let mut lock_path = path.as_ref().as_os_str().to_os_string();
            lock_path.push(".init.lock");
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let file = options
                .open(lock_path)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            fs2::FileExt::lock_exclusive(&file)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            Some(file)
        } else {
            None
        };
        #[cfg(unix)]
        if path.as_ref() != Path::new(":memory:") {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .mode(0o600)
                .open(path.as_ref())
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            if file
                .metadata()
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
                .permissions()
                .mode()
                & 0o777
                != 0o600
            {
                file.set_permissions(std::fs::Permissions::from_mode(0o600))
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            }
        }
        let mut connection = Connection::open(path.as_ref())?;
        connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS documents (collection TEXT NOT NULL, id TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(collection,id)); CREATE INDEX IF NOT EXISTS documents_usage_history_run ON documents(json_extract(value,'$.run_id')) WHERE collection='usage_history'; CREATE INDEX IF NOT EXISTS documents_src_live_task_kind ON documents(json_extract(value,'$.values.task_id'),json_extract(value,'$.values.kind')) WHERE collection='src_live_records'; CREATE INDEX IF NOT EXISTS documents_competition_events_scope ON documents(json_extract(value,'$.competition_id')) WHERE collection='competition_events_live'; CREATE INDEX IF NOT EXISTS documents_competition_events_seq ON documents(json_extract(value,'$.global_seq')) WHERE collection='competition_events_live'; CREATE TABLE IF NOT EXISTS chat_event_clock (id INTEGER PRIMARY KEY CHECK(id=1), head INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS chat_events (seq INTEGER PRIMARY KEY, thread_id TEXT NOT NULL, turn_id TEXT NOT NULL, stream_seq INTEGER NOT NULL, event_id TEXT NOT NULL UNIQUE, event_type TEXT NOT NULL, payload TEXT NOT NULL, UNIQUE(thread_id,stream_seq)); CREATE INDEX IF NOT EXISTS chat_events_turn ON chat_events(thread_id,turn_id,stream_seq); CREATE INDEX IF NOT EXISTS chat_events_stream ON chat_events(thread_id,seq);")?;
        connection.execute_batch(
            "CREATE INDEX IF NOT EXISTS documents_conversation_queue
             ON documents(json_array_length(value,'$.queue'), id)
             WHERE collection='conversation_threads' AND json_array_length(value,'$.queue')>0;",
        )?;
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS conversation_document_versions (
               version INTEGER PRIMARY KEY AUTOINCREMENT, thread_id TEXT NOT NULL UNIQUE
             );
             CREATE TRIGGER IF NOT EXISTS conversation_document_version_insert
             AFTER INSERT ON documents WHEN NEW.collection='conversation_threads' BEGIN
               DELETE FROM conversation_document_versions WHERE thread_id=NEW.id;
               INSERT INTO conversation_document_versions(thread_id) VALUES(NEW.id);
             END;
             CREATE TRIGGER IF NOT EXISTS conversation_document_version_update
             AFTER UPDATE OF value ON documents WHEN NEW.collection='conversation_threads' BEGIN
               DELETE FROM conversation_document_versions WHERE thread_id=NEW.id;
               INSERT INTO conversation_document_versions(thread_id) VALUES(NEW.id);
             END;
             CREATE TRIGGER IF NOT EXISTS conversation_document_version_delete
             AFTER DELETE ON documents WHEN OLD.collection='conversation_threads' BEGIN
               DELETE FROM conversation_document_versions WHERE thread_id=OLD.id;
             END;
             INSERT INTO conversation_document_versions(thread_id)
             SELECT d.id FROM documents d WHERE d.collection='conversation_threads'
               AND NOT EXISTS (
                 SELECT 1 FROM conversation_document_versions v WHERE v.thread_id=d.id
               );
             COMMIT;",
        )?;
        let summary = |doc: &str| {
            format!(
                "json_object('thread',json_extract({doc},'$.thread'),'event_seq',json_extract({doc},'$.event_seq'),\
                 'preview',substr(json_extract({doc},'$.messages[#-1].text'),1,240),\
                 'runtime',json_object('adapter_id',json_extract({doc},'$.runtime.adapter_id'),\
                 'instance_id',json_extract({doc},'$.runtime.instance_id'),'model',json_extract({doc},'$.runtime.model')))"
            )
        };
        connection.execute_batch(&format!(
            "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS conversation_summary_cache (thread_id TEXT PRIMARY KEY, summary TEXT NOT NULL);
             DROP TRIGGER IF EXISTS conversation_summary_insert;
             CREATE TRIGGER conversation_summary_insert AFTER INSERT ON documents WHEN NEW.collection='conversation_threads' BEGIN
               INSERT INTO conversation_summary_cache(thread_id,summary) VALUES(NEW.id,{new})
               ON CONFLICT(thread_id) DO UPDATE SET summary=excluded.summary;
             END;
             DROP TRIGGER IF EXISTS conversation_summary_update;
             CREATE TRIGGER conversation_summary_update AFTER UPDATE OF value ON documents WHEN NEW.collection='conversation_threads' BEGIN
               INSERT INTO conversation_summary_cache(thread_id,summary) VALUES(NEW.id,{new})
               ON CONFLICT(thread_id) DO UPDATE SET summary=excluded.summary;
             END;
             CREATE TRIGGER IF NOT EXISTS conversation_summary_delete AFTER DELETE ON documents WHEN OLD.collection='conversation_threads' BEGIN
               DELETE FROM conversation_summary_cache WHERE thread_id=OLD.id;
             END;
             DELETE FROM conversation_summary_cache WHERE json_type(summary,'$.runtime') IS NULL;
             INSERT OR IGNORE INTO conversation_summary_cache(thread_id,summary)
               SELECT id,{current} FROM documents WHERE collection='conversation_threads';
             COMMIT;",
            new = summary("NEW.value"),
            current = summary("value"),
        ))?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS conversation_share_lookup (
               share_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, token_hash TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS conversation_share_lookup_thread ON conversation_share_lookup(thread_id);
             CREATE TRIGGER IF NOT EXISTS conversation_share_insert AFTER INSERT ON documents WHEN NEW.collection='conversation_share_records' BEGIN
               INSERT INTO conversation_share_lookup(share_id,thread_id,token_hash)
               SELECT json_extract(value,'$.share_id'),NEW.id,json_extract(value,'$.token_hash') FROM json_each(NEW.value,'$.shares');
             END;
             CREATE TRIGGER IF NOT EXISTS conversation_share_update AFTER UPDATE OF value ON documents WHEN NEW.collection='conversation_share_records' BEGIN
               DELETE FROM conversation_share_lookup WHERE thread_id=NEW.id;
               INSERT INTO conversation_share_lookup(share_id,thread_id,token_hash)
               SELECT json_extract(value,'$.share_id'),NEW.id,json_extract(value,'$.token_hash') FROM json_each(NEW.value,'$.shares');
             END;
             CREATE TRIGGER IF NOT EXISTS conversation_share_delete AFTER DELETE ON documents WHEN OLD.collection='conversation_share_records' BEGIN
               DELETE FROM conversation_share_lookup WHERE thread_id=OLD.id;
             END;
             INSERT OR IGNORE INTO conversation_share_lookup(share_id,thread_id,token_hash)
             SELECT json_extract(share.value,'$.share_id'),documents.id,json_extract(share.value,'$.token_hash')
             FROM documents,json_each(documents.value,'$.shares') AS share WHERE documents.collection='conversation_share_records';",
        )?;
        if connection.query_row(
            "SELECT 1 FROM documents WHERE collection='system' AND id='provider_history_visible_v1'",
            [],
            |_| Ok(()),
        ).optional()?.is_none() {
            let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let candidates = {
                let mut stmt = tx.prepare(
                    "SELECT id,value FROM documents WHERE collection='conversation_threads' AND value LIKE '%\"thinking\"%' AND value LIKE '%\"source_content\"%'",
                )?;
                stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            for (id, raw) in candidates {
                let mut document: Value = serde_json::from_str(&raw)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
                if crate::provider_history_import::sanitize_imported_document(&mut document) {
                    tx.execute(
                        "UPDATE documents SET value=?1 WHERE collection='conversation_threads' AND id=?2",
                        params![document.to_string(), id],
                    )?;
                }
            }
            tx.execute(
                "INSERT INTO documents(collection,id,value) VALUES('system','provider_history_visible_v1','true')",
                [],
            )?;
            tx.commit()?;
        }
        let snapshot_connection = if path.as_ref() == Path::new(":memory:") {
            None
        } else {
            Some(Arc::new(Mutex::new(Connection::open_with_flags(
                path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?)))
        };
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            snapshot_connection,
            document_gate:Arc::new(parking_lot::ReentrantMutex::new(())),
            post_commit:Arc::new(Mutex::new(Vec::new())),
        })
    }
    pub(crate) fn read_committed(&self, collection: &str, id: &str) -> rusqlite::Result<Option<Value>> {
        let Some(connection) = &self.snapshot_connection else {
            return self.get(collection, id);
        };
        let raw: Option<String> = connection.lock().map_err(|_| rusqlite::Error::InvalidQuery)?
            .query_row(
                "SELECT value FROM documents WHERE collection=?1 AND id=?2",
                params![collection, id],
                |row| row.get(0),
            )
            .optional()?;
        raw.map(decode).transpose()
    }
    pub(crate) fn read_snapshot<T>(
        &self,
        apply: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        if let Some(connection) = &self.snapshot_connection {
            let mut conn = connection
                .lock()
                .map_err(|_| rusqlite::Error::InvalidQuery)?;
            if conn.is_autocommit() {
                let tx = conn.transaction()?;
                apply(&tx)
            } else {
                apply(&conn)
            }
        } else {
            let _connection_gate = self.document_gate.lock();
            let conn = self
                .connection
                .lock()
                .map_err(|_| rusqlite::Error::InvalidQuery)?;
            apply(&conn)
        }
    }
    pub(crate) fn conversation_exists(&self, id: &str) -> rusqlite::Result<bool> {
        let _connection_gate = self.document_gate.lock();
        let conn = self.connection.lock().map_err(|_| rusqlite::Error::InvalidQuery)?;
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM documents WHERE collection='conversation_threads' AND id=?1)",
            [id],
            |row| row.get(0),
        )
    }
    pub fn get(&self, collection: &str, id: &str) -> rusqlite::Result<Option<Value>> {
        let _connection_gate=self.document_gate.lock();
        let conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        get_locked(&conn, collection, id)
    }
    pub fn list(&self, collection: &str) -> rusqlite::Result<Vec<Value>> {
        let _connection_gate=self.document_gate.lock();
        let conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let mut stmt =
            conn.prepare("SELECT value FROM documents WHERE collection=?1 ORDER BY id")?;
        let rows = stmt.query_map([collection], |row| decode(row.get(0)?))?;
        rows.collect()
    }
    pub fn entries(&self, collection: &str) -> rusqlite::Result<Vec<(String, Value)>> {
        let _connection_gate = self.document_gate.lock();
        let conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let mut stmt =
            conn.prepare("SELECT id, value FROM documents WHERE collection=?1 ORDER BY id")?;
        let rows = stmt.query_map([collection], |row| Ok((row.get(0)?, decode(row.get(1)?)?)))?;
        rows.collect()
    }
    pub fn conversation_share(&self, id: &str, token_hash: &str) -> rusqlite::Result<Option<Value>> {
        let _connection_gate = self.document_gate.lock();
        let conn = self.connection.lock().map_err(|_| rusqlite::Error::InvalidQuery)?;
        let thread: Option<String> = conn.query_row(
            "SELECT thread_id FROM conversation_share_lookup WHERE share_id=?1 AND token_hash=?2",
            params![id, token_hash], |row| row.get(0),
        ).optional()?;
        let Some(thread) = thread else { return Ok(None); };
        conn.query_row(
            "SELECT share.value FROM documents,json_each(documents.value,'$.shares') AS share
             WHERE documents.collection='conversation_share_records' AND documents.id=?1
               AND json_extract(share.value,'$.share_id')=?2",
            params![thread, id], |row| decode(row.get(0)?),
        ).optional()
    }
    pub fn list_competition_events(&self, competition_id: &str) -> rusqlite::Result<Vec<Value>> {
        let _connection_gate = self.document_gate.lock();
        let conn = self.connection.lock().map_err(|_| rusqlite::Error::InvalidQuery)?;
        let mut stmt = conn.prepare("SELECT value FROM documents INDEXED BY documents_competition_events_scope WHERE collection='competition_events_live' AND json_extract(value,'$.competition_id')=?1 ORDER BY id")?;
        let rows = stmt.query_map([competition_id], |row| decode(row.get(0)?))?;
        rows.collect()
    }
    pub fn list_competition_events_after(&self, after: u64, limit: usize) -> rusqlite::Result<Vec<Value>> {
        let _connection_gate = self.document_gate.lock();
        let conn = self.connection.lock().map_err(|_| rusqlite::Error::InvalidQuery)?;
        let mut stmt = conn.prepare("SELECT value FROM documents INDEXED BY documents_competition_events_seq WHERE collection='competition_events_live' AND json_type(value,'$.global_seq')='integer' AND json_extract(value,'$.global_seq')>?1 ORDER BY json_extract(value,'$.global_seq') LIMIT ?2")?;
        let rows = stmt.query_map(params![after.min(i64::MAX as u64) as i64, limit.min(i64::MAX as usize) as i64], |row| decode(row.get(0)?))?;
        rows.collect()
    }
    pub fn list_for_run_ids(
        &self,
        collection: &str,
        run_ids: &HashSet<String>,
    ) -> rusqlite::Result<Vec<Value>> {
        if run_ids.is_empty() {
            return Ok(Vec::new());
        }
        let _connection_gate = self.document_gate.lock();
        let conn = self.connection.lock().map_err(|_| rusqlite::Error::InvalidQuery)?;
        let collection_sql = match collection {
            "usage_history" => "usage_history",
            "usage" => "usage",
            "run_followup_events" => "run_followup_events",
            _ => return Err(rusqlite::Error::InvalidQuery),
        };
        let mut ids: Vec<_> = run_ids.iter().collect();
        ids.sort();
        let placeholders = std::iter::repeat_n("?", ids.len()).collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT value FROM documents WHERE collection='{collection_sql}' AND \
             json_extract(value,'$.run_id') IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(ids), |row| decode(row.get(0)?))?;
        rows.collect()
    }
    pub fn list_src_live_records(&self, task_id: &str, kind: &str) -> rusqlite::Result<Vec<Value>> {
        let _connection_gate = self.document_gate.lock();
        let conn = self.connection.lock().map_err(|_| rusqlite::Error::InvalidQuery)?;
        let sql = if kind.is_empty() {
            "SELECT value FROM documents WHERE collection='src_live_records' AND json_extract(value,'$.values.task_id')=?1"
        } else {
            "SELECT value FROM documents WHERE collection='src_live_records' AND json_extract(value,'$.values.task_id')=?1 AND json_extract(value,'$.values.kind')=?2"
        };
        let mut stmt = conn.prepare(sql)?;
        let args = if kind.is_empty() { vec![task_id] } else { vec![task_id, kind] };
        let rows = stmt.query_map(rusqlite::params_from_iter(args), |row| decode(row.get(0)?))?;
        rows.collect()
    }
    pub fn list_inserted(&self, collection: &str) -> rusqlite::Result<Vec<Value>> {
        let _connection_gate=self.document_gate.lock();
        let conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let mut stmt =
            conn.prepare("SELECT value FROM documents WHERE collection=?1 ORDER BY rowid")?;
        let rows = stmt.query_map([collection], |row| decode(row.get(0)?))?;
        rows.collect()
    }
    pub fn list_inserted_with_rowid(
        &self,
        collection: &str,
    ) -> rusqlite::Result<Vec<(i64, Value)>> {
        let _connection_gate=self.document_gate.lock();
        let conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let mut stmt =
            conn.prepare("SELECT rowid,value FROM documents WHERE collection=?1 ORDER BY rowid")?;
        let rows = stmt.query_map([collection], |row| Ok((row.get(0)?, decode(row.get(1)?)?)))?;
        rows.collect()
    }
    pub fn document_rowid(&self, collection: &str, id: &str) -> rusqlite::Result<Option<i64>> {
        let _connection_gate=self.document_gate.lock();
        let conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        conn.query_row(
            "SELECT rowid FROM documents WHERE collection=?1 AND id=?2",
            params![collection, id],
            |row| row.get(0),
        )
        .optional()
    }
    pub fn command_receipt(&self, command_id: &str) -> rusqlite::Result<Option<Value>> {
        let _connection_gate=self.document_gate.lock();
        let conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        conn.query_row(
            "SELECT json_extract(value,'$.response.receipt') FROM documents WHERE collection IN ('conversation_commands','workspace_commands','runtime_commands','platform_commands') AND json_extract(value,'$.response.receipt.command_id')=?1
             UNION ALL SELECT json_extract(upload.value,'$.response.receipt') FROM documents, json_each(documents.value,'$.upload_commands') AS upload WHERE documents.collection='conversation_threads' AND json_extract(upload.value,'$.response.receipt.command_id')=?1
             UNION ALL SELECT json_extract(command.value,'$.response.receipt') FROM documents, json_each(documents.value,'$.values.commands') AS command WHERE documents.collection='conversation_memory_records' AND json_extract(command.value,'$.response.receipt.command_id')=?1 LIMIT 1",
            [command_id], |row| decode(row.get(0)?),
        ).optional()
    }
    pub fn conversation_summaries(&self) -> rusqlite::Result<Vec<Value>> {
        let _connection_gate=self.document_gate.lock();
        let conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let mut stmt = conn.prepare("SELECT summary FROM conversation_summary_cache ORDER BY thread_id")?;
        stmt.query_map([], |row| decode(row.get(0)?))?.collect()
    }
    pub fn scheduled_conversation_threads(&self) -> rusqlite::Result<Vec<String>> {
        let _connection_gate = self.document_gate.lock();
        let conn = self.connection.lock().map_err(|_| rusqlite::Error::InvalidQuery)?;
        let mut stmt = conn.prepare_cached(
            "SELECT json_extract(d.value,'$.thread.thread_id')
             FROM documents d INDEXED BY documents_conversation_queue
             WHERE d.collection='conversation_threads'
               AND json_array_length(d.value,'$.queue')>0
               AND json_type(d.value,'$.thread.thread_id')='text'
               AND EXISTS (
                 SELECT 1 FROM json_each(d.value,'$.queue') q
                 WHERE json_type(d.value,q.fullkey||'.not_before') IN ('integer','real')
               )
             ORDER BY d.id",
        )?;
        stmt.query_map([], |row| row.get(0))?.collect()
    }
    /// Return the ordered conversation snapshot, fetching content for new versions only.
    pub fn conversation_documents_for_settlement(
        &self,
        known_versions: &[i64],
    ) -> rusqlite::Result<Vec<(i64, Option<Value>)>> {
        let _connection_gate = self.document_gate.lock();
        let conn = self.connection.lock().map_err(|_| rusqlite::Error::InvalidQuery)?;
        let mut stmt = conn.prepare_cached(
            "SELECT v.version,
                    CASE WHEN v.version IN (SELECT value FROM json_each(?1)) THEN NULL ELSE d.value END
             FROM documents d JOIN conversation_document_versions v ON v.thread_id=d.id
             WHERE d.collection='conversation_threads'
             ORDER BY d.id",
        )?;
        let versions = json!(known_versions).to_string();
        stmt.query_map([versions], |row| {
            let document = row.get::<_, Option<String>>(1)?.map(decode).transpose()?;
            Ok((row.get(0)?, document))
        })?
        .collect()
    }
    pub fn chat_event_watermark(&self) -> rusqlite::Result<u64> {
        let _connection_gate = self.document_gate.lock();
        let conn = self.connection.lock().map_err(|_| rusqlite::Error::InvalidQuery)?;
        let head: Option<i64> = conn.query_row("SELECT head FROM chat_event_clock WHERE id=1", [], |row| row.get(0)).optional()?;
        Ok(head.unwrap_or(0).max(0) as u64)
    }
    /// Counters the operations overview reads; every value comes from stored rows.
    pub fn operations_metrics(&self, epoch: &str) -> rusqlite::Result<Value> {
        let _connection_gate=self.document_gate.lock();
        let conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let watermark: i64 = conn
            .query_row("SELECT head FROM chat_event_clock WHERE id=1", [], |row| {
                row.get(0)
            })
            .optional()?
            .unwrap_or(0);
        let gaps: i64 = conn.query_row(
            r#"WITH candidates AS (
                SELECT thread_id,MIN(stream_seq) lo,MAX(stream_seq) hi,COUNT(*) n
                FROM chat_events GROUP BY thread_id
                HAVING MAX(stream_seq)-MIN(stream_seq)+1>COUNT(*)
             ), seqs AS (
                SELECT e.thread_id,e.stream_seq FROM chat_events e JOIN candidates c USING(thread_id)
                UNION SELECT c.thread_id,CAST(json_extract(m.value,'$.stream_seq') AS INTEGER)
                FROM candidates c JOIN documents d ON d.collection='conversation_threads' AND d.id=c.thread_id
                JOIN json_each(d.value,'$.messages') m
                UNION SELECT c.thread_id,CAST(json_extract(a.value,'$.stream_seq') AS INTEGER)
                FROM candidates c JOIN documents d ON d.collection='conversation_threads' AND d.id=c.thread_id
                JOIN json_each(d.value,'$.artifacts') a
             ) SELECT COUNT(*) FROM (
                SELECT c.thread_id FROM candidates c JOIN seqs s USING(thread_id)
                WHERE s.stream_seq BETWEEN c.lo AND c.hi GROUP BY c.thread_id
                HAVING MAX(s.stream_seq)-MIN(s.stream_seq)+1!=COUNT(*)
             )"#,
            [],
            |row| row.get(0),
        )?;
        let mut states = serde_json::Map::new();
        let mut pending_by_type = serde_json::Map::new();
        let mut stmt = conn.prepare(
            "SELECT COALESCE(json_extract(value,'$.response.receipt.state'),'unknown'), COALESCE(json_extract(value,'$.request.command_type'),collection), COUNT(*) FROM documents WHERE collection IN ('conversation_commands','workspace_commands','runtime_commands','platform_commands') AND json_extract(value,'$.response.receipt') IS NOT NULL GROUP BY 1,2",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (state, kind, count) = row?;
            let total = states.get(&state).and_then(Value::as_i64).unwrap_or(0) + count;
            states.insert(state.clone(), json!(total));
            if matches!(state.as_str(), "accepted" | "running" | "waiting") {
                let total = pending_by_type
                    .get(&kind)
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                    + count;
                pending_by_type.insert(kind, json!(total));
            }
        }
        let active_turns: i64 = conn.query_row(
            "SELECT COUNT(*) FROM documents WHERE collection='conversation_threads' AND json_extract(value,'$.thread.state.status')='running' AND json_extract(value,'$.runtime_epoch')=?1",
            [epoch],
            |row| row.get(0),
        )?;
        Ok(json!({
            "event_watermark": watermark,
            "stream_gap_count": gaps,
            "receipts_by_state": states,
            "pending_receipts_by_type": pending_by_type,
            "active_turns": active_turns,
        }))
    }
    /// Threads with a turn running in this runtime epoch, as (thread id, title).
    pub fn active_turn_threads(&self, epoch: &str) -> rusqlite::Result<Vec<(String, String)>> {
        let _connection_gate=self.document_gate.lock();
        let conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let mut stmt = conn.prepare(
            "SELECT d.id, COALESCE(json_extract(c.summary,'$.thread.title'),'')
             FROM conversation_summary_cache c CROSS JOIN documents d
             WHERE json_extract(c.summary,'$.thread.state.status')='running'
               AND d.collection='conversation_threads' AND d.id=c.thread_id
               AND json_extract(d.value,'$.runtime_epoch')=?1
             ORDER BY c.thread_id",
        )?;
        let rows = stmt.query_map([epoch], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect()
    }
    /// Threads with a pending quota hold or resume schedule.
    pub fn quota_threads(&self) -> rusqlite::Result<Vec<String>> {
        let _connection_gate=self.document_gate.lock();
        let conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let mut stmt = conn.prepare(
            "SELECT id FROM documents WHERE collection='conversation_threads' AND (json_extract(value,'$.thread.state.quota_hold') IS NOT NULL OR json_extract(value,'$.thread.state.quota_resume') IS NOT NULL) ORDER BY id",
        )?;
        stmt.query_map([], |row| row.get(0))?.collect()
    }
    pub fn clear_model_results(&self, credential_id: &str) -> rusqlite::Result<()> {
        let _connection_gate=self.document_gate.lock();
        let conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        conn.execute("DELETE FROM documents WHERE json_extract(value,'$.credential_id')=?1 AND (collection='model_test_results' OR (collection='model_catalogs' AND json_extract(value,'$.source')='http_openai_models'))", [credential_id])?;
        Ok(())
    }
    pub fn put(&self, collection: &str, id: &str, value: &Value) -> rusqlite::Result<()> {
        let _connection_gate=self.document_gate.lock();
        let conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        put_locked(&conn, collection, id, value)
    }
    pub fn put_many(&self, records: &[(&str, &str, &Value)]) -> rusqlite::Result<()> {
        let _connection_gate=self.document_gate.lock();
        let mut conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let transaction = document_transaction(&mut conn,rusqlite::TransactionBehavior::Deferred)?;
        for (collection, id, value) in records {
            put_locked(&transaction, collection, id, value)?;
        }
        transaction.commit()
    }
    /// A binding revision and all outstanding grants it replaces change under
    /// one write lock, including grants issued by another server process.
    pub(crate) fn commit_capability_binding(
        &self, key: &str, expected: Option<&Value>, next: &Value, revoke: &[String], at: &str,
    ) -> rusqlite::Result<Option<usize>> {
        let _connection_gate=self.document_gate.lock();
        let mut conn = self.connection.lock().map_err(|_| rusqlite::Error::InvalidQuery)?;
        let tx = document_transaction(&mut conn,rusqlite::TransactionBehavior::Immediate)?;
        if get_locked(&tx,"capability_bindings",key)?.as_ref() != expected { return Ok(None); }
        put_locked(&tx,"capability_bindings",key,next)?;
        let mut count = 0;
        for binding in revoke {
            count += tx.execute("UPDATE documents SET value=json_set(value,'$.revoked_at',?1) WHERE collection='capability_grants' AND json_extract(value,'$.binding_id')=?2 AND json_extract(value,'$.revoked_at') IS NULL",params![at,binding])?;
        }
        tx.commit()?;
        Ok(Some(count))
    }
    pub(crate) fn revoke_capability_sessions(&self, thread_id: Option<&str>, session_id: Option<&str>, at: &str) -> rusqlite::Result<usize> {
        let _connection_gate=self.document_gate.lock();
        let mut conn = self.connection.lock().map_err(|_|rusqlite::Error::InvalidQuery)?;
        let tx = document_transaction(&mut conn,rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("UPDATE documents SET value=json_set(value,'$.open',json('false'),'$.closed_at',?3) WHERE collection='capability_sessions' AND (json_extract(value,'$.thread_id')=?1 OR id=?2)",params![thread_id,session_id,at])?;
        let count = tx.execute("UPDATE documents SET value=json_set(value,'$.revoked_at',?3) WHERE collection='capability_grants' AND json_extract(value,'$.revoked_at') IS NULL AND (json_extract(value,'$.thread_id')=?1 OR json_extract(value,'$.agent_session_id')=?2)",params![thread_id,session_id,at])?;
        tx.commit()?;
        Ok(count)
    }
    /// Commit ordinary documents only while each previously read value is still current.
    /// The immediate transaction makes this comparison shared by all server processes.
    pub fn put_many_if(
        &self,
        expected: &[(&str, &str, Option<&Value>)],
        records: &[(&str, &str, &Value)],
    ) -> rusqlite::Result<bool> {
        let _connection_gate=self.document_gate.lock();
        let mut conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let transaction =
            document_transaction(&mut conn,rusqlite::TransactionBehavior::Immediate)?;
        for (collection, id, value) in expected {
            if get_locked(&transaction, collection, id)?.as_ref() != *value {
                return Ok(false);
            }
        }
        for (collection, id, value) in records {
            put_locked(&transaction, collection, id, value)?;
        }
        transaction.commit()?;
        Ok(true)
    }
    /// Commit conditional document writes and deletes together. The delete plan
    /// is evaluated under the same immediate SQLite transaction so callers can
    /// inspect related rows before deciding which records to remove.
    pub fn put_many_if_with_deletes<F>(
        &self,
        expected: &[(&str, &str, Option<&Value>)],
        records: &[(&str, &str, &Value)],
        plan_deletes: F,
    ) -> rusqlite::Result<bool>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<Option<Vec<(String, String)>>>,
    {
        let _connection_gate=self.document_gate.lock();
        let mut conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let transaction =
            document_transaction(&mut conn,rusqlite::TransactionBehavior::Immediate)?;
        for (collection, id, value) in expected {
            if get_locked(&transaction, collection, id)?.as_ref() != *value {
                return Ok(false);
            }
        }
        let Some(deletes) = plan_deletes(&transaction)? else {
            return Ok(false);
        };
        for (collection, id) in deletes {
            transaction.execute(
                "DELETE FROM documents WHERE collection=?1 AND id=?2",
                params![collection, id],
            )?;
        }
        for (collection, id, value) in records {
            put_locked(&transaction, collection, id, value)?;
        }
        transaction.commit()?;
        Ok(true)
    }
    /// Record a competition command and its planned events in one transaction.
    /// The caller may commit its external-effect outbox in a later transaction.
    pub fn put_many_if_with_competition_events(
        &self,
        expected: &[(&str, &str, Option<&Value>)],
        records: &[(&str, &str, &Value)],
        versions_collection: &str,
        events_collection: &str,
        competition_id: &str,
        events: &[(Value, u64)],
    ) -> rusqlite::Result<Option<Vec<Value>>> {
        let _connection_gate=self.document_gate.lock();
        let mut conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let transaction =
            document_transaction(&mut conn,rusqlite::TransactionBehavior::Immediate)?;
        for (collection, id, value) in expected {
            if get_locked(&transaction, collection, id)?.as_ref() != *value {
                return Ok(None);
            }
        }
        for (collection, id, value) in records {
            put_locked(&transaction, collection, id, value)?;
        }
        let mut seq: u64 = transaction.query_row(
            "SELECT COALESCE(MAX(json_extract(value,'$.seq')),0) FROM documents WHERE collection=?1 AND json_extract(value,'$.competition_id')=?2",
            params![events_collection,competition_id], |row| row.get(0))?;
        let mut global_seq: u64 = transaction.query_row(
            "SELECT COALESCE(MAX(json_extract(value,'$.global_seq')),0) FROM documents WHERE collection=?1",
            [events_collection], |row| row.get(0))?;
        let mut saved = Vec::with_capacity(events.len());
        for (event, floor) in events {
            seq += 1;
            global_seq += 1;
            let mut event = event.clone();
            event["seq"] = json!(seq);
            event["global_seq"] = json!(global_seq);
            enrich_competition_event(&transaction,&mut event)?;
            let aggregate_type = event["aggregate_type"]
                .as_str()
                .ok_or(rusqlite::Error::InvalidQuery)?;
            let aggregate_id = event["aggregate_id"]
                .as_str()
                .ok_or(rusqlite::Error::InvalidQuery)?;
            let version_key = format!("{aggregate_type}\0{aggregate_id}");
            let previous = get_locked(&transaction, versions_collection, &version_key)?
                .and_then(|row| row["version"].as_u64())
                .unwrap_or(0);
            put_locked(
                &transaction,
                versions_collection,
                &version_key,
                &json!({
                    "aggregate_type":aggregate_type,"aggregate_id":aggregate_id,
                    "version":previous.max(*floor).saturating_add(1),"updated_at":event["occurred_at"],
                }),
            )?;
            put_locked(
                &transaction,
                events_collection,
                &format!("{competition_id}:{seq:020}"),
                &event,
            )?;
            saved.push(event);
        }
        transaction.commit()?;
        Ok(Some(saved))
    }
    pub fn advance_version_with_record(
        &self,
        collection: &str,
        id: &str,
        aggregate_type: &str,
        aggregate_id: &str,
        floor: u64,
        updated_at: &str,
        record: Option<(&str, &str, &Value)>,
    ) -> rusqlite::Result<u64> {
        let _connection_gate=self.document_gate.lock();
        let mut conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let transaction = document_transaction(&mut conn,rusqlite::TransactionBehavior::Deferred)?;
        let prior = get_locked(&transaction, collection, id)?
            .and_then(|row| row["version"].as_u64())
            .unwrap_or(0);
        let next = prior.max(floor).saturating_add(1);
        put_locked(
            &transaction,
            collection,
            id,
            &json!({
                "aggregate_type":aggregate_type,"aggregate_id":aggregate_id,
                "version":next,"updated_at":updated_at,
            }),
        )?;
        if let Some((record_collection, record_id, value)) = record {
            put_locked(&transaction, record_collection, record_id, value)?;
        }
        transaction.commit()?;
        Ok(next)
    }
    /// Allocate the competition and global event sequence under the same SQLite
    /// write lock as the aggregate version and event insertion.
    pub fn append_competition_event(
        &self,
        versions_collection: &str,
        events_collection: &str,
        competition_id: &str,
        aggregate_type: &str,
        aggregate_id: &str,
        floor: u64,
        mut event: Value,
    ) -> rusqlite::Result<Value> {
        let _connection_gate=self.document_gate.lock();
        let mut conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let transaction =
            document_transaction(&mut conn,rusqlite::TransactionBehavior::Immediate)?;
        let seq: u64 = transaction.query_row(
            "SELECT COALESCE(MAX(json_extract(value,'$.seq')),0)+1 FROM documents WHERE collection=?1 AND json_extract(value,'$.competition_id')=?2",
            params![events_collection,competition_id], |row| row.get(0))?;
        let global_seq: u64 = transaction.query_row(
            "SELECT COALESCE(MAX(json_extract(value,'$.global_seq')),0)+1 FROM documents WHERE collection=?1",
            [events_collection], |row| row.get(0))?;
        event["seq"] = json!(seq);
        event["global_seq"] = json!(global_seq);
        enrich_competition_event(&transaction,&mut event)?;
        let key = format!("{aggregate_type}\0{aggregate_id}");
        let previous = get_locked(&transaction, versions_collection, &key)?
            .and_then(|row| row["version"].as_u64())
            .unwrap_or(0);
        let next = previous.max(floor).saturating_add(1);
        put_locked(
            &transaction,
            versions_collection,
            &key,
            &json!({
                "aggregate_type":aggregate_type,"aggregate_id":aggregate_id,
                "version":next,"updated_at":event["occurred_at"],
            }),
        )?;
        put_locked(
            &transaction,
            events_collection,
            &format!("{competition_id}:{seq:020}"),
            &event,
        )?;
        transaction.commit()?;
        Ok(event)
    }
    pub fn delete(&self, collection: &str, id: &str) -> rusqlite::Result<bool> {
        let _connection_gate=self.document_gate.lock();
        let conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        Ok(conn.execute(
            "DELETE FROM documents WHERE collection=?1 AND id=?2",
            params![collection, id],
        )? > 0)
    }

    /// Permanent conversation deletion keeps the global event clock but removes
    /// this thread's log and command payloads as well as its current document.
    pub(crate) fn purge_conversation(&self, id: &str) -> rusqlite::Result<bool> {
        let _gate = self.document_gate.lock();
        let mut connection = self.connection.lock().map_err(|_| rusqlite::Error::InvalidQuery)?;
        let tx = document_transaction(&mut connection, rusqlite::TransactionBehavior::Immediate)?;
        let deleted = tx.execute("DELETE FROM documents WHERE collection='conversation_threads' AND id=?1", [id])? > 0;
        tx.execute("DELETE FROM chat_events WHERE thread_id=?1", [id])?;
        tx.execute("DELETE FROM documents WHERE collection IN ('control_threads','control_index_positions','conversation_share_records') AND id=?1", [id])?;
        tx.execute("DELETE FROM documents WHERE collection='conversation_commands' AND (instr(id,?1)=1 OR instr(id,?2)=1)",
            params![format!("thread:{id}:"),format!("idem:thread:{id}:")])?;
        tx.commit()?;
        Ok(deleted)
    }
    pub fn put_if_absent(
        &self,
        collection: &str,
        id: &str,
        value: &Value,
    ) -> rusqlite::Result<bool> {
        let _connection_gate=self.document_gate.lock();
        let conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let value=seal_src_artifact(collection,value)?;
        Ok(conn.execute(
            "INSERT OR IGNORE INTO documents(collection,id,value) VALUES (?1,?2,?3)",
            params![collection, id, value.to_string()],
        )? > 0)
    }
    pub fn compare_and_put(
        &self,
        collection: &str,
        id: &str,
        version: u64,
        values: Value,
    ) -> rusqlite::Result<Result<Value, Value>> {
        let _connection_gate=self.document_gate.lock();
        let mut conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let tx = document_transaction(&mut conn,rusqlite::TransactionBehavior::Immediate)?;
        let current =
            get_locked(&tx, collection, id)?.unwrap_or_else(|| json!({"version":0,"values":{}}));
        if current.get("version").and_then(Value::as_u64) != Some(version) {
            return Ok(Err(current));
        }
        let Some(next) = version.checked_add(1) else {
            return Err(rusqlite::Error::InvalidQuery);
        };
        let saved = json!({"version":next,"values":values});
        put_locked(&tx, collection, id, &saved)?;
        tx.commit()?;
        Ok(Ok(saved))
    }
    /// Compare every record before writing any of them, then commit the batch
    /// in one SQLite transaction. A stale version leaves the whole batch intact.
    pub fn compare_and_put_many(
        &self,
        records: &[(&str, &str, u64, Value)],
    ) -> rusqlite::Result<Result<Vec<Value>, Value>> {
        let _connection_gate=self.document_gate.lock();
        let mut conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let tx = document_transaction(&mut conn,rusqlite::TransactionBehavior::Immediate)?;
        let mut seen = HashSet::new();
        for (collection, id, version, _) in records {
            if !seen.insert((*collection, *id)) {
                return Err(rusqlite::Error::InvalidQuery);
            }
            let current = get_locked(&tx, collection, id)?
                .unwrap_or_else(|| json!({"version":0,"values":{}}));
            if current["version"].as_u64() != Some(*version) {
                return Ok(Err(current));
            }
        }
        let mut saved = Vec::with_capacity(records.len());
        for (collection, id, version, values) in records {
            let next = version
                .checked_add(1)
                .ok_or(rusqlite::Error::InvalidQuery)?;
            let row = json!({"version":next,"values":values});
            put_locked(&tx, collection, id, &row)?;
            saved.push(row);
        }
        tx.commit()?;
        Ok(Ok(saved))
    }
    pub fn save_account(
        &self,
        id: &str,
        value: Value,
        create_only: bool,
        expected: Option<&str>,
    ) -> rusqlite::Result<Result<Value, ()>> {
        self.write_account(id, value, create_only, expected, true)
    }
    /// Save an account whose material was replaced as a whole, so secrets of
    /// the previous version are not carried into it.
    pub fn replace_account(&self, id: &str, value: Value) -> rusqlite::Result<Result<Value, ()>> {
        self.write_account(id, value, false, None, false)
    }
    pub(crate) fn write_account(
        &self,
        id: &str,
        mut value: Value,
        create_only: bool,
        expected: Option<&str>,
        carry_secrets: bool,
    ) -> rusqlite::Result<Result<Value, ()>> {
        let _connection_gate=self.document_gate.lock();
        let mut conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let tx = document_transaction(&mut conn,rusqlite::TransactionBehavior::Immediate)?;
        let account = get_locked(&tx, "credential_accounts", id)?;
        let legacy = get_locked(&tx, "model_endpoints", id)?;
        if account
            .as_ref()
            .zip(legacy.as_ref())
            .is_some_and(|(a, b)| !same_http_identity(a, b))
        {
            return Ok(Err(()));
        }
        let old = account.or(legacy);
        if (create_only && old.is_some())
            || expected.is_some_and(|rev| {
                old.as_ref()
                    .and_then(|v| v.get("revision"))
                    .and_then(Value::as_str)
                    != Some(rev)
            })
        {
            return Ok(Err(()));
        }
        if let (Some(old), Some(obj), true) = (old.as_ref(), value.as_object_mut(), carry_secrets) {
            for (key,previous) in old.as_object().into_iter().flatten() {
                if !obj.contains_key(key) { obj.insert(key.clone(),previous.clone()); }
            }
            for key in ["secret", "codex_auth_json"] {
                if obj
                    .get(key)
                    .is_none_or(|v| v.is_null() || v.as_str().is_some_and(|v| v.trim().is_empty()))
                {
                    if let Some(v) = old.get(key) {
                        obj.insert(key.to_owned(), v.clone());
                    }
                }
            }
        }
        let unchanged = old.as_ref().is_some_and(|previous|account_material(previous,false)==account_material(&value,false));
        let preserve_evidence = old.as_ref().is_some_and(|previous|account_material(previous,true)==account_material(&value,true));
        let revision = if unchanged {
            old.as_ref().and_then(|row|row["revision"].as_str()).map(str::to_owned)
        } else { None }.unwrap_or_else(||uuid::Uuid::new_v4().simple().to_string());
        value["revision"] = json!(revision);
        if unchanged { value["updated_at"] = old.as_ref().map(|row|row["updated_at"].clone()).unwrap_or(Value::Null); }
        put_locked(&tx, "credential_accounts", id, &value)?;
        tx.execute(
            "DELETE FROM documents WHERE collection='model_endpoints' AND id=?1",
            [id],
        )?;
        if preserve_evidence { advance_account_results(&tx,id,&revision)?; }
        else { clear_account_results(&tx,id)?; }
        tx.commit()?;
        Ok(Ok(value))
    }
    /// Current references only; imported snapshots and turn attribution stay intact.
    pub fn credential_usage(&self, id: &str) -> rusqlite::Result<Vec<Value>> {
        let _connection_gate=self.document_gate.lock();
        let conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        Ok(credential_changes(&conn, id, false)?.0)
    }
    /// One request reads current selections once, then derives every account's usages in memory.
    pub fn credential_usage_index(
        &self,
        ids: &[String],
    ) -> rusqlite::Result<std::collections::BTreeMap<String, Vec<Value>>> {
        let _connection_gate=self.document_gate.lock();
        let conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let rows = reference_documents(&conn, false)?;
        let mut index = std::collections::BTreeMap::new();
        for id in ids {
            let scoped = credential_scope(&conn, id)?;
            index.insert(id.clone(), reference_changes(rows.clone(), &scoped).0);
        }
        Ok(index)
    }
    pub fn delete_account(
        &self,
        id: &str,
        detach: bool,
    ) -> rusqlite::Result<Result<Option<usize>, Vec<Value>>> {
        self.delete_account_with_material(id,detach,|_|Ok(()))
    }
    pub(crate) fn delete_account_with_material<F>(
        &self,id: &str,detach: bool,prepare: F,
    ) -> rusqlite::Result<Result<Option<usize>,Vec<Value>>>
    where F: FnOnce(bool)->rusqlite::Result<()> {
        let _connection_gate=self.document_gate.lock();
        let mut conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let tx = document_transaction(&mut conn,rusqlite::TransactionBehavior::Immediate)?;
        let endpoint = id.starts_with("endpoint:");
        let id = id.strip_prefix("endpoint:").unwrap_or(id);
        let account = get_locked(&tx, "credential_accounts", id)?;
        let legacy = get_locked(&tx, "model_endpoints", id)?;
        let collision = account
            .as_ref()
            .zip(legacy.as_ref())
            .is_some_and(|(a, b)| !same_http_identity(a, b));
        if account.is_none() && legacy.is_none() {
            return Ok(Ok(None));
        }
        let selected = if endpoint {
            format!("endpoint:{id}")
        } else {
            id.to_owned()
        };
        let (references, changes) = credential_changes(&tx, &selected, true)?;
        let enabled: Vec<_> = references
            .iter()
            .filter(|row| row["enabled"] != false)
            .cloned()
            .collect();
        if !detach && !enabled.is_empty() {
            return Ok(Err(enabled));
        }
        if detach {
            for (collection, key, value) in changes {
                put_locked(&tx, &collection, &key, &value)?;
            }
        }
        prepare(account.is_some() && (!collision || !endpoint))?;
        if collision {
            let collection = if endpoint {
                "model_endpoints"
            } else {
                "credential_accounts"
            };
            tx.execute(
                "DELETE FROM documents WHERE collection=?1 AND id=?2",
                params![collection, id],
            )?;
        } else {
            tx.execute("DELETE FROM documents WHERE collection IN ('credential_accounts','model_endpoints') AND id=?1", [id])?;
        }
        clear_account_results(&tx, id)?;
        tx.commit()?;
        Ok(Ok(Some(if detach { references.len() } else { 0 })))
    }
    /// Endpoint edits use shared account storage and retire the old current alias.
    pub fn save_http_endpoint(
        &self,
        id: &str,
        patch: &Value,
    ) -> rusqlite::Result<Result<(Value, bool), ()>> {
        let _connection_gate=self.document_gate.lock();
        let mut conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let tx = document_transaction(&mut conn,rusqlite::TransactionBehavior::Immediate)?;
        let account = get_locked(&tx, "credential_accounts", id)?;
        let legacy = get_locked(&tx, "model_endpoints", id)?;
        if account
            .as_ref()
            .zip(legacy.as_ref())
            .is_some_and(|(a, b)| !same_http_identity(a, b))
        {
            return Ok(Err(()));
        }
        let old = account.or(legacy);
        let existed = old.is_some();
        let previous = old.clone();
        let mut value = old.unwrap_or_else(|| json!({"engine":"api","worker_engine":"api"}));
        for key in ["base_url", "provider", "models", "default_model"] {
            value[key] = patch[key].clone();
        }
        if patch["secret"]
            .as_str()
            .is_some_and(|v| !v.trim().is_empty())
        {
            value["secret"] = patch["secret"].clone();
        }
        value["account_id"] = json!(id);
        value["id"] = json!(format!("account:{id}"));
        value["connection"] = json!("custom_endpoint");
        let unchanged = previous.as_ref().is_some_and(|old|account_material(old,false)==account_material(&value,false));
        let preserve = previous.as_ref().is_some_and(|old|account_material(old,true)==account_material(&value,true));
        let revision = if unchanged { previous.as_ref().and_then(|row|row["revision"].as_str()).map(str::to_owned) }
            else { None }.unwrap_or_else(||uuid::Uuid::new_v4().simple().to_string());
        value["revision"] = json!(revision);
        if !unchanged { value["updated_at"] = json!(chrono::Utc::now().timestamp_millis() as f64 / 1000.0); }
        put_locked(&tx, "credential_accounts", id, &value)?;
        tx.execute("DELETE FROM documents WHERE collection='model_endpoints' AND id=?1",[id])?;
        if preserve { advance_account_results(&tx,id,&revision)?; } else { clear_account_results(&tx,id)?; }
        tx.commit()?;
        Ok(Ok((value, existed)))
    }
    pub fn installation_id(&self) -> rusqlite::Result<String> {
        let _connection_gate=self.document_gate.lock();
        let mut conn = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let tx = document_transaction(&mut conn,rusqlite::TransactionBehavior::Immediate)?;
        let id = match get_locked(&tx, "system", "installation_id")? {
            Some(Value::String(id)) => id,
            Some(_) => return Err(rusqlite::Error::InvalidQuery),
            None => {
                let id: String =
                    tx.query_row("SELECT lower(hex(randomblob(16)))", [], |r| r.get(0))?;
                put_locked(&tx, "system", "installation_id", &Value::String(id.clone()))?;
                id
            }
        };
        tx.commit()?;
        Ok(id)
    }
}
pub(crate) fn same_http_identity(a: &Value, b: &Value) -> bool {
    ["base_url", "secret", "default_model", "models", "provider"]
        .iter()
        .all(|key| {
            let av = &a[*key];
            let bv = &b[*key];
            av == bv
                || ((av.is_null() || av.as_str() == Some(""))
                    && (bv.is_null() || bv.as_str() == Some("")))
        })
}
fn account_material(row: &Value, test: bool) -> Value {
    let mut material = row.as_object().cloned().unwrap_or_default();
    for key in ["revision","updated_at","created_at","label","present","account_id","id","material_generation"] { material.remove(key); }
    if test {
        for key in ["default_model","target_model","models","model_capabilities","provider"] { material.remove(key); }
    }
    material.retain(|_,value|!value.is_null() && value.as_str()!=Some(""));
    Value::Object(material)
}
fn advance_account_results(conn: &Connection,id: &str,revision: &str) -> rusqlite::Result<()> {
    for alias in [format!("account:{id}"),format!("endpoint:{id}")] {
        conn.execute("UPDATE documents SET value=json_set(value,'$.configuration_revision',?2)
            WHERE collection IN ('model_test_results','model_catalogs') AND json_extract(value,'$.credential_id')=?1",
            params![alias,revision])?;
    }
    Ok(())
}
fn clear_account_results(conn: &Connection, id: &str) -> rusqlite::Result<()> {
    for alias in [format!("account:{id}"), format!("endpoint:{id}")] {
        conn.execute("DELETE FROM documents WHERE collection IN ('model_test_results','model_catalogs') AND json_extract(value,'$.credential_id')=?1", [alias])?;
    }
    Ok(())
}
fn account_reference(value: &str, id: &str) -> bool {
    if let Some(id) = id.strip_prefix("only_endpoint:") {
        return value == format!("endpoint:{id}");
    }
    if let Some(id) = id.strip_prefix("only_account:") {
        return value == format!("account:{id}")
            || value == format!("secret://credential-accounts/{id}");
    }
    if id.starts_with("system:") {
        return value == id;
    }
    value == format!("account:{id}")
        || value == format!("endpoint:{id}")
        || value == format!("secret://credential-accounts/{id}")
}
fn raw_account_reference(value: &str, id: &str) -> bool {
    !id.starts_with("only_endpoint:") && value == id.strip_prefix("only_account:").unwrap_or(id)
}
fn identity_aliases(value: &Value, id: &str, aliases: &mut Vec<String>) {
    match value {
        Value::Object(row) => {
            if row
                .get("secret_ref")
                .and_then(Value::as_str)
                .is_some_and(|v| raw_account_reference(v, id) || account_reference(v, id))
            {
                if let Some(alias) = row.get("id").and_then(Value::as_str) {
                    aliases.push(alias.to_owned());
                }
            }
            for child in row.values() {
                identity_aliases(child, id, aliases);
            }
        }
        Value::Array(rows) => {
            for child in rows {
                identity_aliases(child, id, aliases);
            }
        }
        _ => {}
    }
}
fn expand_identity_aliases(value: &mut Value, id: &str, aliases: &[String]) {
    match value {
        Value::Object(row) => {
            if row
                .get("credential_id")
                .and_then(Value::as_str)
                .is_some_and(|v| aliases.iter().any(|a| a == v))
            {
                row.insert(
                    "credential_id".into(),
                    json!(format!(
                        "account:{}",
                        id.strip_prefix("only_account:").unwrap_or(id)
                    )),
                );
            }
            for child in row.values_mut() {
                expand_identity_aliases(child, id, aliases);
            }
        }
        Value::Array(rows) => {
            for child in rows {
                expand_identity_aliases(child, id, aliases);
            }
        }
        _ => {}
    }
}
fn reference_rows(
    value: &Value,
    id: &str,
    kind: &str,
    path: &str,
    enabled: bool,
    refs: &mut Vec<Value>,
) {
    match value {
        Value::Object(row) => {
            let enabled = enabled && row.get("enabled") != Some(&json!(false));
            let direct = row.iter().any(|(key, value)| {
                value.as_str().is_some_and(|value| match key.as_str() {
                    "credential_id" | "credentialId" | "endpoint_id" => {
                        account_reference(value, id)
                    }
                    "credential_account" | "secret_ref" => {
                        raw_account_reference(value, id) || account_reference(value, id)
                    }
                    _ => false,
                })
            });
            if direct {
                let key = row
                    .get("id")
                    .or_else(|| row.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or(path);
                let label = row.get("label").and_then(Value::as_str).unwrap_or(key);
                refs.push(json!({"kind":kind,"id":key,"label":label,"enabled":enabled,"engine":row.get("engine").cloned().unwrap_or(json!("")),"model":row.get("model").cloned().unwrap_or(json!("")),"roles":row.get("roles").cloned().unwrap_or(json!([]))}));
            }
            for (key, child) in row {
                if key == "credentials" {
                    continue;
                }
                let child_kind = match key.as_str() {
                    "seats" | "worker_profiles" => "worker",
                    "llm_profiles" => "llm_profile",
                    _ => kind,
                };
                let child_path = if kind == "llm_profile" {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                reference_rows(child, id, child_kind, &child_path, enabled, refs);
            }
        }
        Value::Array(rows) => {
            for (index, row) in rows.iter().enumerate() {
                reference_rows(row, id, kind, &format!("{path}.{index}"), enabled, refs);
            }
        }
        _ => {}
    }
}
fn detach_fields(value: &mut Value, id: &str) -> bool {
    let mut changed = false;
    match value {
        Value::Object(row) => {
            let direct = row.iter().any(|(key, value)| {
                value.as_str().is_some_and(|value| match key.as_str() {
                    "credential_id" | "credentialId" | "endpoint_id" => {
                        account_reference(value, id)
                    }
                    "credential_account" | "secret_ref" => {
                        raw_account_reference(value, id) || account_reference(value, id)
                    }
                    _ => false,
                })
            });
            if direct {
                for key in [
                    "credential_id",
                    "credentialId",
                    "endpoint_id",
                    "secret_ref",
                    "credential_account",
                ] {
                    if row.contains_key(key) {
                        row.insert(key.into(), json!(""));
                    }
                }
                row.remove("credential_kind");
                if row.contains_key("enabled") {
                    row.insert("enabled".into(), json!(false));
                }
                if row.contains_key("endpoint_id") {
                    row.insert("connection".into(), json!("default"));
                    row.insert("base_url".into(), json!(""));
                }
                changed = true;
            }
            for (key, child) in row.iter_mut() {
                if key == "defaultModel"
                    && child["credentialId"]
                        .as_str()
                        .is_some_and(|value| account_reference(value, id))
                {
                    *child = Value::Null;
                    changed = true;
                } else {
                    changed |= detach_fields(child, id);
                }
            }
        }
        Value::Array(rows) => {
            for row in rows {
                changed |= detach_fields(row, id);
            }
        }
        _ => {}
    }
    changed
}
type ReferenceDocument = (String, String, Value);
fn credential_scope(conn: &Connection, id: &str) -> rusqlite::Result<String> {
    let endpoint = id.starts_with("endpoint:");
    let raw_id = id.strip_prefix("endpoint:").unwrap_or(id);
    let account = get_locked(conn, "credential_accounts", raw_id)?;
    let legacy = get_locked(conn, "model_endpoints", raw_id)?;
    let scoped = if account
        .as_ref()
        .zip(legacy.as_ref())
        .is_some_and(|(a, b)| !same_http_identity(a, b))
    {
        format!(
            "only_{}:{raw_id}",
            if endpoint { "endpoint" } else { "account" }
        )
    } else {
        raw_id.to_owned()
    };
    Ok(scoped)
}
fn reference_documents(
    conn: &Connection,
    full_documents: bool,
) -> rusqlite::Result<Vec<ReferenceDocument>> {
    let sql = if full_documents {
        "SELECT collection,id,value FROM documents WHERE collection IN ('conversation_threads','settings','seats','worker_profiles','credentials') ORDER BY collection,id"
    } else {
        "SELECT collection,id,CASE WHEN collection='conversation_threads' THEN json_extract(value,'$.thread.title','$.runtime','$.queue') ELSE value END FROM documents WHERE collection IN ('conversation_threads','settings','seats','worker_profiles','credentials') ORDER BY collection,id"
    };
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt
        .query_map([], |row| {
            let collection = row.get::<_, String>(0)?;
            let key = row.get::<_, String>(1)?;
            let mut value = decode(row.get(2)?)?;
            if !full_documents && collection == "conversation_threads" {
                value = json!({"thread":{"title":value[0]},"runtime":value[1],"queue":value[2]});
            }
            Ok((collection, key, value))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !rows
        .iter()
        .any(|(c, k, _)| c == "settings" && k == "worker_templates")
    {
        if let Some(config) = get_locked(conn, "import_metadata", "_worker_config.json")? {
            rows.push(("settings".into(), "worker_templates".into(), config));
        }
    }
    let configured_collections: Vec<String> = rows
        .iter()
        .find(|(c, k, _)| c == "settings" && k == "worker_templates")
        .map(|(_, _, value)| {
            ["seats", "worker_profiles", "credentials"]
                .into_iter()
                .filter(|key| value.get(*key).is_some())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    rows.retain(|(collection, _, _)| !configured_collections.contains(collection));
    Ok(rows)
}
fn reference_changes(
    rows: Vec<ReferenceDocument>,
    id: &str,
) -> (Vec<Value>, Vec<ReferenceDocument>) {
    let mut refs = Vec::new();
    let mut changes = Vec::new();
    let mut aliases = Vec::new();
    for (collection, _, value) in &rows {
        if collection != "conversation_threads" {
            identity_aliases(value, id, &mut aliases);
        }
    }
    for (collection, key, mut value) in rows {
        if collection == "conversation_threads" {
            let mut changed = detach_fields(&mut value["runtime"], id);
            if let Some(queue) = value["queue"].as_array_mut() {
                for item in queue {
                    changed |= detach_fields(&mut item["runtime"], id);
                }
            }
            if changed {
                refs.push(json!({"kind":"conversation","id":key,"label":value["thread"]["title"],"enabled":true}));
                if value["queue"].as_array().is_some_and(|q| !q.is_empty()) {
                    value["thread"]["state"]["queue_paused"] = json!(true);
                    value["thread"]["state"]["queue_pause_reason"] = json!("credential_removed");
                }
                changes.push((collection, key, value));
            }
        } else {
            expand_identity_aliases(&mut value, id, &aliases);
            let mut current_refs = Vec::new();
            if collection != "credentials" {
                reference_rows(
                    &value,
                    id,
                    if collection == "settings" {
                        "settings"
                    } else {
                        "worker"
                    },
                    &key,
                    true,
                    &mut current_refs,
                );
            }
            if detach_fields(&mut value, id) {
                if let Some(version) = value["version"].as_u64() {
                    value["version"] = json!(version + 1);
                }
                refs.extend(current_refs);
                changes.push((collection, key, value));
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    refs.retain(|row| {
        seen.insert((
            row["kind"].as_str().unwrap_or("").to_owned(),
            row["id"].as_str().unwrap_or("").to_owned(),
        ))
    });
    (refs, changes)
}
fn credential_changes(
    conn: &Connection,
    id: &str,
    full_documents: bool,
) -> rusqlite::Result<(Vec<Value>, Vec<ReferenceDocument>)> {
    let scoped = credential_scope(conn, id)?;
    Ok(reference_changes(
        reference_documents(conn, full_documents)?,
        &scoped,
    ))
}

fn decode(raw: String) -> rusqlite::Result<Value> {
    serde_json::from_str(&raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}
fn get_locked(conn: &Connection, collection: &str, id: &str) -> rusqlite::Result<Option<Value>> {
    conn.query_row(
        "SELECT value FROM documents WHERE collection=?1 AND id=?2",
        params![collection, id],
        |row| decode(row.get(0)?),
    )
    .optional()
}
fn put_locked(
    conn: &Connection,
    collection: &str,
    id: &str,
    value: &Value,
) -> rusqlite::Result<()> {
    let value=seal_src_artifact(collection,value)?;
    conn.execute("INSERT INTO documents(collection,id,value) VALUES (?1,?2,?3) ON CONFLICT(collection,id) DO UPDATE SET value=excluded.value", params![collection,id,value.to_string()])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conversation_summaries_cover_legacy_rows_and_follow_thread_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.sqlite");
        let legacy = Connection::open(&path).unwrap();
        legacy.execute_batch("CREATE TABLE documents (collection TEXT NOT NULL, id TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(collection,id))").unwrap();
        legacy.execute("INSERT INTO documents(collection,id,value) VALUES('conversation_threads','thread-1',?1)",
            [json!({"thread":{"thread_id":"thread-1","title":"original"},"event_seq":1,"messages":[{"text":"before"}]}).to_string()]).unwrap();
        drop(legacy);

        let store = Store::open(&path).unwrap();
        assert_eq!(store.conversation_summaries().unwrap()[0]["preview"], "before");
        store.put("conversation_threads", "thread-1", &json!({"thread":{"thread_id":"thread-1","title":"updated"},"event_seq":2,"messages":[{"text":"after"}]})).unwrap();
        let summaries = store.conversation_summaries().unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0]["thread"]["title"], "updated");
        assert_eq!(summaries[0]["event_seq"], 2);
        assert_eq!(summaries[0]["preview"], "after");
        store.delete("conversation_threads", "thread-1").unwrap();
        assert!(store.conversation_summaries().unwrap().is_empty());
    }

    #[test]
    fn lease_claim_and_event_sequences_are_shared_between_sqlite_connections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lease.sqlite");
        let first = Store::open(&path).unwrap();
        let second = Store::open(&path).unwrap();
        let one = json!({"lease_id":"one"});
        let two = json!({"lease_id":"two"});
        assert!(
            first
                .put_many_if(
                    &[("heads", "challenge", None)],
                    &[("heads", "challenge", &one)]
                )
                .unwrap()
        );
        assert!(
            !second
                .put_many_if(
                    &[("heads", "challenge", None)],
                    &[("heads", "challenge", &two)]
                )
                .unwrap()
        );
        let handles = [first,second].into_iter().map(|store|std::thread::spawn(move || {
            store.append_competition_event("versions","events","competition","lease","same",0,
                json!({"competition_id":"competition","aggregate_type":"lease","aggregate_id":"same","occurred_at":"2026-01-01T00:00:00Z"})).unwrap()
        })).collect::<Vec<_>>();
        let mut sequences = handles
            .into_iter()
            .map(|handle| handle.join().unwrap()["seq"].as_u64().unwrap())
            .collect::<Vec<_>>();
        sequences.sort();
        assert_eq!(sequences, vec![1, 2]);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.list("events").unwrap().len(), 2);
        assert_eq!(
            store.get("versions", "lease\0same").unwrap().unwrap()["version"],
            2
        );
    }
    #[test]
    fn accepted_competition_plan_rolls_back_with_its_planned_event() {
        let store = Store::open(":memory:").unwrap();
        store
            .connection
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_event BEFORE INSERT ON documents \
             WHEN NEW.collection='events' BEGIN SELECT RAISE(ABORT, 'test event failure'); END;",
            )
            .unwrap();
        let command = json!({"request":{"command_type":"instance.ensure"},"response":{"receipt":{"state":"accepted"}}});
        let event = json!({"event_type":"competition.instance.lease_requested","occurred_at":"2026-01-01T00:00:00Z",
            "competition_id":"comp","aggregate_type":"instance_lease","aggregate_id":"lease"});
        assert!(
            store
                .put_many_if_with_competition_events(
                    &[("commands", "cmd", None)],
                    &[("commands", "cmd", &command)],
                    "versions",
                    "events",
                    "comp",
                    &[(event.clone(), 0)],
                )
                .is_err()
        );
        assert!(store.get("commands", "cmd").unwrap().is_none());
        assert!(
            store
                .get("versions", "instance_lease\0lease")
                .unwrap()
                .is_none()
        );
        store
            .connection
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER reject_event;")
            .unwrap();
        let saved = store
            .put_many_if_with_competition_events(
                &[("commands", "cmd", None)],
                &[("commands", "cmd", &command)],
                "versions",
                "events",
                "comp",
                &[(event, 0)],
            )
            .unwrap()
            .unwrap();
        assert_eq!(saved[0]["seq"], 1);
        assert_eq!(
            store.get("commands", "cmd").unwrap().unwrap()["response"]["receipt"]["state"],
            "accepted"
        );
    }
    #[test]
    fn put_many_rolls_back_all_records_when_one_write_fails() {
        let store = Store::open(":memory:").unwrap();
        store
            .connection
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_second_record BEFORE INSERT ON documents \
             WHEN NEW.id='reject' BEGIN SELECT RAISE(ABORT, 'test failure'); END;",
            )
            .unwrap();
        let first = json!({"state":"pending"});
        let second = json!({"state":"processing"});
        assert!(
            store
                .put_many(&[("outbox", "first", &first), ("outbox", "reject", &second)])
                .is_err()
        );
        assert!(store.get("outbox", "first").unwrap().is_none());
        assert!(store.get("outbox", "reject").unwrap().is_none());
        assert!(
            store
                .advance_version_with_record(
                    "versions",
                    "competition\\0sample",
                    "competition",
                    "sample",
                    0,
                    "2026-01-01T00:00:00Z",
                    Some(("outbox", "reject", &second)),
                )
                .is_err()
        );
        assert!(
            store
                .get("versions", "competition\\0sample")
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn credential_delete_detaches_current_bindings_without_rewriting_history() {
        let store = Store::open(":memory:").unwrap();
        store
            .put(
                "credential_accounts",
                "sample",
                &json!({"secret":"synthetic"}),
            )
            .unwrap();
        let document = json!({"thread":{"thread_id":"chat","title":"Chat","state":{}},"runtime":{"credential_id":"account:sample","model":"model"},"messages":[{"text":"Keep me"}],"turns":[{"runtime_snapshot":{"credential_id":"account:sample"}}]});
        store
            .put("conversation_threads", "chat", &document)
            .unwrap();
        let imported = json!({"llm_profiles":{"titler":{"endpoint_id":"endpoint:sample","model":"model"}},"seats":[{"id":"disabled","enabled":false,"credential_id":"account:sample"}]});
        store
            .put("import_metadata", "_worker_config.json", &imported)
            .unwrap();
        let index = store
            .credential_usage_index(&["sample".into(), "endpoint:sample".into(), "missing".into()])
            .unwrap();
        assert_eq!(index["sample"], store.credential_usage("sample").unwrap());
        assert_eq!(index["endpoint:sample"], index["sample"]);
        assert!(index["missing"].is_empty());
        let references = store.delete_account("sample", false).unwrap().unwrap_err();
        assert_eq!(references.len(), 2);
        assert!(
            references
                .iter()
                .any(|row| row["kind"] == "llm_profile" && row["id"] == "titler")
        );
        assert_eq!(
            store.get("conversation_threads", "chat").unwrap().unwrap(),
            document
        );
        assert!(
            store
                .delete_account("missing", true)
                .unwrap()
                .unwrap()
                .is_none()
        );
        assert!(store.get("settings", "worker_templates").unwrap().is_none());
        assert!(
            store
                .delete_account("sample", true)
                .unwrap()
                .unwrap()
                .unwrap()
                == 3
        );
        let saved = store.get("conversation_threads", "chat").unwrap().unwrap();
        assert_eq!(saved["runtime"]["credential_id"], "");
        assert_eq!(saved["turns"], document["turns"]);
        assert_eq!(saved["messages"], document["messages"]);
        assert_eq!(
            store
                .get("import_metadata", "_worker_config.json")
                .unwrap()
                .unwrap(),
            imported
        );
        assert_eq!(
            store.get("settings", "worker_templates").unwrap().unwrap()["llm_profiles"]["titler"]["endpoint_id"],
            ""
        );
    }
    #[test]
    fn credential_disabled_references_do_not_block_delete() {
        let store = Store::open(":memory:").unwrap();
        store
            .put(
                "credential_accounts",
                "sample",
                &json!({"secret":"synthetic"}),
            )
            .unwrap();
        store
            .put(
                "settings",
                "worker_templates",
                &json!({"seats":[{"id":"seat","enabled":false,"credential_id":"account:sample"}]}),
            )
            .unwrap();
        assert!(
            store
                .delete_account("sample", false)
                .unwrap()
                .unwrap()
                .is_some()
        );
    }
    #[test]
    fn credential_endpoint_alias_edit_and_collision_preserve_identity() {
        let store = Store::open(":memory:").unwrap();
        let legacy = json!({"id":"endpoint:sample","secret":"synthetic","base_url":"https://example.invalid/v1","models":["one"],"default_model":"one"});
        store.put("model_endpoints", "sample", &legacy).unwrap();
        let patch = json!({"secret":"","base_url":"https://example.invalid/v1","provider":"","models":["two"],"default_model":"two"});
        let (saved, updated) = store.save_http_endpoint("sample", &patch).unwrap().unwrap();
        assert!(updated);
        assert_eq!(saved["secret"], legacy["secret"]);
        assert!(store.get("model_endpoints", "sample").unwrap().is_none());
        let account = crate::provider::matching_account(&store, "account:sample")
            .unwrap()
            .unwrap();
        assert_eq!(
            crate::provider::matching_account(&store, "endpoint:sample")
                .unwrap()
                .unwrap(),
            account
        );
        let mut collision = legacy.clone();
        collision["base_url"] = json!("https://other.invalid/v1");
        store.put("model_endpoints", "sample", &collision).unwrap();
        assert!(store.save_http_endpoint("sample", &patch).unwrap().is_err());
        let index = store
            .credential_usage_index(&["sample".into(), "endpoint:sample".into()])
            .unwrap();
        assert_eq!(index["sample"], store.credential_usage("sample").unwrap());
        assert_eq!(
            index["endpoint:sample"],
            store.credential_usage("endpoint:sample").unwrap()
        );
        assert_eq!(
            store.get("credential_accounts", "sample").unwrap().unwrap(),
            account
        );
        assert!(
            store
                .delete_account("sample", true)
                .unwrap()
                .unwrap()
                .is_some()
        );
        assert_eq!(
            store.get("model_endpoints", "sample").unwrap().unwrap(),
            collision
        );
        assert!(
            store
                .delete_account("endpoint:sample", true)
                .unwrap()
                .unwrap()
                .is_some()
        );
        assert!(
            crate::provider::matching_account(&store, "endpoint:sample")
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn persists_documents_and_rejects_stale_preference_writes() {
        let path = std::env::temp_dir().join(format!(
            "muteki-store-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let identity;
        {
            let store = Store::open(&path).unwrap();
            identity = store.installation_id().unwrap();
            let saved = store
                .compare_and_put("settings", "ui", 0, json!({"theme":"dark"}))
                .unwrap()
                .unwrap();
            assert_eq!(saved["version"], 1);
            assert_eq!(
                store
                    .compare_and_put("settings", "ui", 0, json!({"theme":"light"}))
                    .unwrap()
                    .unwrap_err(),
                saved
            );
            store.put("runs", "one", &json!({"run_id":"one"})).unwrap();
        }
        {
            let store = Store::open(&path).unwrap();
            assert_eq!(store.installation_id().unwrap(), identity);
            assert_eq!(
                store.get("settings", "ui").unwrap().unwrap()["values"]["theme"],
                "dark"
            );
            assert_eq!(store.list("runs").unwrap().len(), 1);
            assert!(store.delete("runs", "one").unwrap());
            assert!(store.list("runs").unwrap().is_empty());
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn multi_cas_rejects_a_stale_record_without_writing_the_others() {
        let store = Store::open(":memory:").unwrap();
        let first = store
            .compare_and_put_many(&[
                ("monitor", "cycle", 0, json!({"state":"scheduled"})),
                ("monitor", "current", 0, json!({"cycle":"cycle"})),
            ])
            .unwrap()
            .unwrap();
        assert_eq!(first[0]["version"], 1);
        assert!(
            store
                .compare_and_put_many(&[
                    ("monitor", "cycle", 1, json!({"state":"running"})),
                    ("monitor", "current", 0, json!({"cycle":"other"})),
                ])
                .unwrap()
                .is_err()
        );
        assert_eq!(
            store.get("monitor", "cycle").unwrap().unwrap()["values"]["state"],
            "scheduled"
        );
        assert_eq!(
            store.get("monitor", "current").unwrap().unwrap()["values"]["cycle"],
            "cycle"
        );
    }

    #[test]
    fn empty_claim_actor_stays_empty_while_a_missing_actor_stays_system() {
        let store = Store::open(":memory:").unwrap();
        store.put("platform_command_claims", "i14-empty", &json!({
            "command_id":"i14-empty",
            "event_context":{"actor_id":""}
        })).unwrap();
        store.put("platform_commands", "i14-empty", &json!({
            "request":{"actor":{"id":""}}
        })).unwrap();
        store.put("platform_command_claims", "i14-named", &json!({
            "command_id":"i14-named",
            "event_context":{"actor_id":"operator-1"}
        })).unwrap();
        let empty = store.append_competition_event(
            "competition_versions", "competition_events_live", "comp-i14",
            "competition", "comp-i14", 0,
            json!({"competition_id":"comp-i14","command_id":"i14-empty","aggregate_type":"competition","aggregate_id":"comp-i14","event_type":"competition.sync.requested","payload":{}}),
        ).unwrap();
        assert_eq!(empty["actor_id"], json!(""));
        let named = store.append_competition_event(
            "competition_versions", "competition_events_live", "comp-i14",
            "competition", "comp-i14", 0,
            json!({"competition_id":"comp-i14","command_id":"i14-named","aggregate_type":"competition","aggregate_id":"comp-i14","event_type":"competition.sync.requested","payload":{}}),
        ).unwrap();
        assert_eq!(named["actor_id"], json!("operator-1"));
        let absent = store.append_competition_event(
            "competition_versions", "competition_events_live", "comp-i14",
            "competition", "comp-i14", 0,
            json!({"competition_id":"comp-i14","aggregate_type":"competition","aggregate_id":"comp-i14","event_type":"competition.sync.requested","payload":{}}),
        ).unwrap();
        assert_eq!(absent["actor_id"], json!("system"));
    }
}

fn enrich_competition_event(conn:&Connection,event:&mut Value)->rusqlite::Result<()>{
    let command_id=event["command_id"].as_str().unwrap_or("").to_owned();
    let claim=if command_id.is_empty(){None}else{get_locked(conn,"platform_command_claims",&command_id)?};
    let command=if command_id.is_empty(){None}else{get_locked(conn,"platform_commands",&command_id)?};
    let context=claim.as_ref().and_then(|row|row.get("event_context"));
    let fallback=command.as_ref().and_then(|row|row["request"]["actor"]["id"].as_str().filter(|v|!v.is_empty()).or_else(||row["plan"]["actor_id"].as_str().filter(|v|!v.is_empty()))).unwrap_or("system");
    let actor=match context.and_then(|row|row.get("actor_id")){
        Some(raw) if !raw.is_null()=>raw.as_str().unwrap_or("").to_owned(),
        _=>fallback.to_owned(),
    };
    if event["actor_id"].is_null(){event["actor_id"]=json!(actor);}
    event["producer"]=json!("builtin.competition");
    for field in ["causation_id","correlation_id","idempotency_key"]{
        if event[field].is_null(){event[field]=context.and_then(|row|row.get(field)).cloned().unwrap_or_else(||if field!="idempotency_key" && !command_id.is_empty(){json!(command_id)}else{command.as_ref().map(|row|row["request"][field].clone()).unwrap_or(Value::Null)});}
    }
    event["payload"]["competition_id"]=event["competition_id"].clone();
    event["event_id"]=json!(format!("evt-comp-{}",event["global_seq"]));Ok(())
}

enum DocumentTransaction<'a>{Transaction(rusqlite::Transaction<'a>),Savepoint(rusqlite::Savepoint<'a>)}
impl std::ops::Deref for DocumentTransaction<'_>{type Target=Connection;fn deref(&self)->&Connection{match self{Self::Transaction(tx)=>tx,Self::Savepoint(tx)=>tx}}}
impl DocumentTransaction<'_>{fn commit(self)->rusqlite::Result<()>{match self{Self::Transaction(tx)=>tx.commit(),Self::Savepoint(tx)=>tx.commit()}}}
fn document_transaction(conn:&mut Connection,behavior:rusqlite::TransactionBehavior)->rusqlite::Result<DocumentTransaction<'_>>{
    if conn.is_autocommit(){conn.transaction_with_behavior(behavior).map(DocumentTransaction::Transaction)}else{conn.savepoint().map(DocumentTransaction::Savepoint)}
}
struct DocumentRollback<'a>{store:&'a Store,name:String,outer:bool,armed:bool}
impl Drop for DocumentRollback<'_>{fn drop(&mut self){if self.armed{if let Ok(mut batches)=self.store.post_commit.lock(){batches.pop();}if let Ok(conn)=self.store.connection.lock(){let sql=if self.outer{"ROLLBACK".into()}else{format!("ROLLBACK TO {}; RELEASE {}",self.name,self.name)};let _=conn.execute_batch(&sql);}}}}

fn seal_src_artifact(collection:&str,value:&Value)->rusqlite::Result<Value>{
    let mut value=value.clone();
    if collection=="src_artifacts"&&value["sha256"].is_null(){
        use base64::Engine;use sha2::{Digest,Sha256};
        let raw=base64::engine::general_purpose::STANDARD.decode(value["data"].as_str().ok_or(rusqlite::Error::InvalidQuery)?).map_err(|_|rusqlite::Error::InvalidQuery)?;
        value["sha256"]=json!(format!("{:x}",Sha256::digest(&raw)));value["integrity_version"]=json!(1);
    }Ok(value)
}
