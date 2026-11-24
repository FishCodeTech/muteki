//! The Turso database file behind [`super::SessionStore`]: opening it, the
//! single writer connection, reader connections, and the checkpoint that
//! leaves the main file self-contained.
//!
//! Turso's API is async but drives its own I/O, so every call is run to
//! completion with `block_on` on the calling (blocking) thread.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, Ordering},
};

use futures_lite::future::block_on;
use turso::{Builder, Connection, Database, Row, Value};

/// `PRAGMA user_version` of a completed store. A staged database keeps 0
/// until its last write, so a file at the final name with 0 was never
/// finished, and a larger value was written by a newer Tcode.
pub(super) const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE projects (id TEXT PRIMARY KEY, body BLOB NOT NULL);
CREATE TABLE sessions (id TEXT PRIMARY KEY, body BLOB NOT NULL);
CREATE TABLE events (
    session_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    line BLOB NOT NULL,
    PRIMARY KEY (session_id, position)
);
";

/// Worktrees whose thread was deleted with "keep": the orphan sweep leaves
/// them. Added to version 1 without a version change, since an older build
/// that ignores the table loses nothing, so every open creates it if missing.
pub(super) const KEPT_WORKTREES: &str =
    "CREATE TABLE IF NOT EXISTS kept_worktrees (path TEXT PRIMARY KEY)";

/// A failure after which the connection or database can no longer be
/// trusted; the store stops serving every handle once it sees one.
#[derive(Debug)]
pub(super) struct Broken(pub(super) String);

impl std::fmt::Display for Broken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Broken {}

pub(super) fn is_broken(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|inner| inner.is::<Broken>())
}

pub(super) struct Db {
    path: PathBuf,
    cancelled: AtomicBool,
    pinned: Mutex<Vec<Weak<Mutex<Option<Connection>>>>>,
    database: Database,
    /// Every write goes through this one connection, one transaction at a
    /// time. Turso's `busy_timeout` spins the CPU, so a second writer is never
    /// allowed to contend instead.
    writer: Mutex<Connection>,
    /// Idle reader connections. Readers see the last committed snapshot and
    /// never wait for the writer.
    readers: Mutex<Vec<Connection>>,
}

impl Db {
    /// Open an existing database file, or create an empty one with the schema
    /// when `create` is set (only ever on the staging path).
    pub(super) fn open(path: &Path, create: bool) -> io::Result<Self> {
        if !create && !path.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} does not exist", path.display()),
            ));
        }
        let location = path.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a UTF-8 path", path.display()),
            )
        })?;
        let database = block_on(Builder::new_local(location).build())
            .map_err(|error| open_error(path, error))?;
        let writer = database
            .connect()
            .map_err(|error| sql_error(path, "connect", error))?;
        let db = Self {
            path: path.to_path_buf(),
            cancelled: AtomicBool::new(false),
            pinned: Mutex::default(),
            database,
            writer: Mutex::new(writer),
            readers: Mutex::new(Vec::new()),
        };
        {
            let writer = db.writer()?;
            // FULL syncs the WAL on every commit; on Apple platforms only
            // F_FULLFSYNC (`fullfsync`) also flushes the drive's write cache,
            // so a committed batch survives power loss. Both are read back
            // because Turso accepts and ignores values it does not implement.
            db.execute(&writer, "PRAGMA synchronous = FULL", ())?;
            db.execute(&writer, "PRAGMA fullfsync = ON", ())?;
            let synchronous = db.query_integer(&writer, "PRAGMA synchronous")?;
            if synchronous != Some(2) {
                return Err(io::Error::other(format!(
                    "{}: PRAGMA synchronous is {synchronous:?}, expected FULL",
                    path.display()
                )));
            }
            #[cfg(target_vendor = "apple")]
            if db.query_integer(&writer, "PRAGMA fullfsync")? != Some(1) {
                return Err(io::Error::other(format!(
                    "{}: PRAGMA fullfsync did not take effect",
                    path.display()
                )));
            }
            if create {
                block_on(writer.execute_batch(SCHEMA))
                    .map_err(|error| sql_error(path, "create schema", error))?;
            }
        }
        Ok(db)
    }

    pub(super) fn connect_reader(&self) -> io::Result<Connection> {
        self.database
            .connect()
            .map_err(|error| sql_error(&self.path, "connect reader", error))
    }

    pub(super) fn pin_reader(&self, connection: Connection) -> Arc<Mutex<Option<Connection>>> {
        let reader = Arc::new(Mutex::new(Some(connection)));
        let mut pinned = self.pinned.lock().unwrap();
        pinned.retain(|reader| reader.strong_count() > 0);
        pinned.push(Arc::downgrade(&reader));
        reader
    }

    pub(super) fn cancel_reads(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub(super) fn close_readers(&self) {
        for reader in self
            .pinned
            .lock()
            .unwrap()
            .drain(..)
            .filter_map(|reader| reader.upgrade())
        {
            if let Some(connection) = reader.lock().unwrap().take() {
                let _ = self.execute(&connection, "ROLLBACK", ());
            }
        }
        self.readers.lock().unwrap().clear();
    }

    fn writer(&self) -> io::Result<std::sync::MutexGuard<'_, Connection>> {
        self.writer
            .lock()
            .map_err(|_| io::Error::other(Broken("the store writer panicked".into())))
    }

    /// Run `body` in one IMMEDIATE transaction on the writer connection and
    /// commit it. Any failure rolls back explicitly; a connection whose
    /// rollback cannot be confirmed is reported [`Broken`] and never reused.
    pub(super) fn write<R>(
        &self,
        body: impl FnOnce(&Self, &Connection) -> io::Result<R>,
    ) -> io::Result<R> {
        let writer = self.writer()?;
        self.execute(&writer, "BEGIN IMMEDIATE", ())?;
        let result = body(self, &writer)
            .and_then(|value| self.execute(&writer, "COMMIT", ()).map(|_| value));
        if let Err(error) = result {
            self.roll_back(&writer)?;
            return Err(error);
        }
        result
    }

    fn roll_back(&self, writer: &Connection) -> io::Result<()> {
        let autocommit = |connection: &Connection| {
            connection
                .is_autocommit()
                .map_err(|error| sql_error(&self.path, "inspect transaction", error))
        };
        if !autocommit(writer)? {
            block_on(writer.execute("ROLLBACK", ())).map_err(|error| {
                io::Error::other(Broken(format!(
                    "{}: rollback failed: {error}",
                    self.path.display()
                )))
            })?;
        }
        if !autocommit(writer)? {
            return Err(io::Error::other(Broken(format!(
                "{}: a transaction is still open after rollback",
                self.path.display()
            ))));
        }
        Ok(())
    }

    /// Run `body` on an idle reader connection. A connection that saw an
    /// error is dropped rather than returned to the pool.
    pub(super) fn read<R>(
        &self,
        body: impl FnOnce(&Self, &Connection) -> io::Result<R>,
    ) -> io::Result<R> {
        let pooled = self
            .readers
            .lock()
            .ok()
            .and_then(|mut readers| readers.pop());
        let connection = match pooled {
            Some(connection) => connection,
            None => self
                .database
                .connect()
                .map_err(|error| sql_error(&self.path, "connect", error))?,
        };
        let result = body(self, &connection);
        if result.is_ok()
            && let Ok(mut readers) = self.readers.lock()
        {
            readers.push(connection);
        }
        result
    }

    /// Execute one statement to completion; its statement is finished before
    /// this returns, so the next statement never meets an active one.
    pub(super) fn execute(
        &self,
        connection: &Connection,
        sql: &str,
        params: impl turso::IntoParams,
    ) -> io::Result<u64> {
        block_on(async {
            // Cached: migration and batches run the same few statements for
            // every line.
            let mut statement = connection.prepare_cached(sql).await?;
            statement.execute(params).await
        })
        .map_err(|error| sql_error(&self.path, sql, error))
    }

    /// Run a query and hand every row to `each`, draining the statement so it
    /// is finished before this returns.
    pub(super) fn query(
        &self,
        connection: &Connection,
        sql: &str,
        params: impl turso::IntoParams,
        mut each: impl FnMut(&Row) -> io::Result<()>,
    ) -> io::Result<()> {
        self.query_while(connection, sql, params, |row| each(row).map(|()| true))
    }

    /// Stop a read when its consumer has enough rows. Reset even on an early
    /// stop so a pooled connection never keeps the statement's read snapshot.
    pub(super) fn query_while(
        &self,
        connection: &Connection,
        sql: &str,
        params: impl turso::IntoParams,
        mut each: impl FnMut(&Row) -> io::Result<bool>,
    ) -> io::Result<()> {
        block_on(async {
            let mut statement = connection
                .prepare_cached(sql)
                .await
                .map_err(|error| sql_error(&self.path, sql, error))?;
            let mut rows = statement
                .query(params)
                .await
                .map_err(|error| sql_error(&self.path, sql, error))?;
            while let Some(row) = rows
                .next()
                .await
                .map_err(|error| sql_error(&self.path, sql, error))?
            {
                if self.cancelled.load(Ordering::Acquire) && sql.starts_with("SELECT") {
                    drop(rows);
                    statement
                        .reset()
                        .map_err(|error| sql_error(&self.path, sql, error))?;
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "store read cancelled",
                    ));
                }
                if !each(&row)? {
                    drop(rows);
                    statement
                        .reset()
                        .map_err(|error| sql_error(&self.path, sql, error))?;
                    break;
                }
            }
            Ok(())
        })
    }

    fn query_integer(&self, connection: &Connection, sql: &str) -> io::Result<Option<i64>> {
        let mut value = None;
        self.query(connection, sql, (), |row| {
            value = Some(integer(row, 0)?);
            Ok(())
        })?;
        Ok(value)
    }

    pub(super) fn user_version(&self) -> io::Result<i64> {
        let writer = self.writer()?;
        Ok(self
            .query_integer(&writer, "PRAGMA user_version")?
            .unwrap_or(0))
    }

    /// Copy every committed frame into the main file and truncate the WAL,
    /// failing unless both are confirmed: after this the main file alone is
    /// the database.
    pub(super) fn checkpoint(&self) -> io::Result<()> {
        let writer = self.writer()?;
        let mut busy = None;
        self.query(&writer, "PRAGMA wal_checkpoint(TRUNCATE)", (), |row| {
            busy = Some(integer(row, 0)?);
            Ok(())
        })?;
        if busy != Some(0) {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                format!(
                    "{}: checkpoint did not complete (busy = {busy:?})",
                    self.path.display()
                ),
            ));
        }
        let wal = wal_path(&self.path);
        match std::fs::metadata(&wal) {
            Ok(metadata) if metadata.len() > 0 => Err(io::Error::other(format!(
                "{} still holds {} bytes after a truncating checkpoint",
                wal.display(),
                metadata.len()
            ))),
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

pub(super) fn wal_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push("-wal");
    PathBuf::from(name)
}

pub(super) fn integer(row: &Row, index: usize) -> io::Result<i64> {
    match row.get_value(index) {
        Ok(Value::Integer(value)) => Ok(value),
        Ok(other) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected an integer column, found {other:?}"),
        )),
        Err(error) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            error.to_string(),
        )),
    }
}

pub(super) fn blob(row: &Row, index: usize) -> io::Result<Vec<u8>> {
    match row.get_value(index) {
        Ok(Value::Blob(bytes)) => Ok(bytes),
        Ok(other) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected a blob column, found {other:?}"),
        )),
        Err(error) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            error.to_string(),
        )),
    }
}

pub(super) fn text(row: &Row, index: usize) -> io::Result<String> {
    match row.get_value(index) {
        Ok(Value::Text(text)) => Ok(text),
        Ok(other) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected a text column, found {other:?}"),
        )),
        Err(error) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            error.to_string(),
        )),
    }
}

fn open_error(path: &Path, error: turso::Error) -> io::Error {
    let message = error.to_string();
    // Turso holds an exclusive lock on the file for as long as a process has
    // it open; the message is the only signal its error carries.
    if message.contains("locked by another process") {
        return io::Error::new(
            io::ErrorKind::ResourceBusy,
            format!(
                "{} is in use by another Tcode process: {message}",
                path.display()
            ),
        );
    }
    match error {
        turso::Error::IoError(kind, operation) => io::Error::new(
            kind,
            format!("could not open {} ({operation}): {message}", path.display()),
        ),
        _ => io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "could not open the session database {}: {message}. The file is kept as it \
                 is; it can be examined with the sqlite3 command-line tool.",
                path.display()
            ),
        ),
    }
}

pub(super) fn sql_error(path: &Path, operation: &str, error: turso::Error) -> io::Error {
    let kind = match &error {
        turso::Error::IoError(kind, _) => *kind,
        turso::Error::DatabaseFull(_) => io::ErrorKind::StorageFull,
        turso::Error::Busy(_) | turso::Error::BusySnapshot(_) => io::ErrorKind::ResourceBusy,
        turso::Error::Corrupt(_) | turso::Error::NotAdb(_) => io::ErrorKind::InvalidData,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, format!("{}: {operation}: {error}", path.display()))
}
