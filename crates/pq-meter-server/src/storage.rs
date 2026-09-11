//! Durable, per-gateway persistence for meter readings received over the tunnel.
//!
//! Unlike `pq-meter-client`'s [`MeterStore`](../../pq-meter-client/src/storage.rs)
//! (a transient replay buffer, safe to delete and recreate — see
//! `DESIGN_DECISIONS.md`), [`ServerStore`] is the durable, queryable copy this
//! project keeps history in: the natural place for analysis, and what a
//! Grafana dashboard reads directly (see `crates/pq-meter-server/TODO.md`).
//!
//! Three differences from the client's store follow from that:
//!
//! * **Value columns are nullable and grow additively.** A reading naming a
//!   column the table doesn't have yet gets `ALTER TABLE ADD COLUMN`; a
//!   reading missing a column the table already has stores `NULL` for it.
//!   Nothing is ever dropped or backfilled with a fabricated default — `NULL`
//!   honestly means "this gateway didn't report that value", which is also
//!   what lets two different meter kinds share one file.
//! * **The pull cursor is stored, not derived.** `MAX(idx)` cannot express "a
//!   gateway's history was reset", since the old rows still hold the high
//!   value; [`ServerStore::cursor`] and the cursor half of
//!   [`ServerStore::record_batch`] track it explicitly per gateway.
//! * **Rows are keyed by `(gateway, idx, ts_millis)`, not `(gateway, idx)`.**
//!   A gateway whose local database was deleted and recreated restarts its
//!   own row ids at 1; keying on `(gateway, idx)` alone would make those
//!   fresh rows collide with — and be silently dropped as duplicates of — the
//!   previous run's rows 1..N. The timestamp tells the two runs apart while
//!   still collapsing a genuine re-pull of the same rows to one copy.
//!
//! Reading names arrive over the network (from whichever gateway holds a
//! `CONNECT` tunnel), so — unlike the client's own trusted `&'static str`
//! adapter code — [`record_batch`](ServerStore::record_batch) validates them
//! before using them as SQL column names, and caps how many distinct columns
//! one database file will grow to.
//!
//! One [`ServerStore`] (behind a lock) is shared by every tunnel session —
//! `rusqlite::Connection` is not `Sync`, so callers serialize access rather
//! than each opening their own; concurrent connections independently
//! initializing the same fresh WAL-mode file raced each other into spurious
//! `SQLITE_IOERR`s. WAL mode itself is what lets a Grafana reader touch the
//! file without blocking the writer.
//!
//! A reader outside this process (Grafana, reading the file directly — see
//! `docker-compose.yml`) only ever sees data that has been checkpointed from
//! the WAL into the base file: WAL-mode's cross-connection visibility relies
//! on a memory-mapped `-shm` index file, and that kind of shared-memory
//! coherency does not reliably survive a Docker Desktop bind mount. Frequent
//! checkpointing (see [`ServerStore::checkpoint`]) keeps that window small by
//! moving fresh rows into ordinary pages a plain file read picks up fine,
//! without depending on `-shm` coherency at all.

use std::collections::HashSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension};

/// Largest number of distinct value columns `readings` is allowed to grow to.
/// Reading names come off the network; without a cap, a misbehaving or
/// hostile gateway sending an unbounded variety of field names could grow the
/// table's schema without limit.
const MAX_VALUE_COLUMNS: usize = 64;

/// Column names reserved for `readings`' own bookkeeping; a reading may not
/// report a value under one of these names.
const RESERVED_COLUMN_NAMES: [&str; 4] = ["id", "gateway", "idx", "ts_millis"];

/// One measurement received from a gateway, ready to store.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// The gateway's own row id for this measurement (the wire protocol's
    /// `"index"`), used for dedupe together with `ts_millis`.
    pub idx: u64,
    /// When the reading was taken, as Unix epoch milliseconds.
    pub ts_millis: i64,
    /// The measured values, by name.
    pub values: Vec<(String, f64)>,
}

/// How many rows a [`ServerStore::record_batch`] call actually added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Recorded {
    /// Rows that were new.
    pub inserted: usize,
    /// Rows already present under the same `(gateway, idx, ts_millis)`, so
    /// this call left them untouched.
    pub duplicates: usize,
}

/// A handle to the server's durable SQLite store.
pub struct ServerStore {
    conn: Connection,
}

impl ServerStore {
    /// Opens (creating if needed) the store at `path`, creating its tables on
    /// first use.
    pub fn open(path: &Path) -> Result<Self> {
        Self::from_connection(Connection::open(path)?)
    }

    /// Opens an in-memory database. For tests.
    #[cfg(test)]
    fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        // WAL lets a writer and a reader (another tunnel session, or Grafana)
        // work at the same time; the busy timeout keeps a brief overlap from
        // surfacing as an error.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5_000)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS readings (
                 id        INTEGER PRIMARY KEY AUTOINCREMENT,
                 gateway   TEXT NOT NULL,
                 idx       INTEGER NOT NULL,
                 ts_millis INTEGER NOT NULL
             );
             CREATE UNIQUE INDEX IF NOT EXISTS uniq_readings
                 ON readings (gateway, idx, ts_millis);
             CREATE INDEX IF NOT EXISTS idx_readings_gateway_ts
                 ON readings (gateway, ts_millis);
             CREATE TABLE IF NOT EXISTS gateways (
                 gateway    TEXT PRIMARY KEY,
                 cursor     INTEGER NOT NULL,
                 updated_ms INTEGER NOT NULL
             );",
        )?;
        Ok(ServerStore { conn })
    }

    /// The pull cursor stored for `gateway` — the index its next `"data"`
    /// request should ask "everything after" — or `0` if this gateway has
    /// never been seen.
    pub fn cursor(&self, gateway: &str) -> Result<u64> {
        let cursor: Option<i64> = self
            .conn
            .query_row(
                "SELECT cursor FROM gateways WHERE gateway = ?1",
                [gateway],
                |row| row.get(0),
            )
            .optional()?;
        Ok(cursor.unwrap_or(0) as u64)
    }

    /// Records one batch of measurements from `gateway` and advances its
    /// stored cursor to `new_cursor`, in one transaction: a crash partway
    /// through cannot leave the cursor persisted without the rows it
    /// describes, or the other way around.
    ///
    /// Rows are inserted with `INSERT OR IGNORE`, so re-delivering a batch
    /// the store already has (the second line of defence TODO item 4 asks
    /// for) is a no-op rather than a duplicate.
    pub fn record_batch(&mut self, gateway: &str, new_cursor: u64, rows: &[Row]) -> Result<Recorded> {
        let tx = self.conn.transaction()?;

        for row in rows {
            ensure_columns(&tx, &row.values)?;
        }

        let mut inserted = 0usize;
        for row in rows {
            let names: Vec<&str> = row.values.iter().map(|(name, _)| name.as_str()).collect();
            let placeholders: Vec<String> = (0..names.len()).map(|i| format!("?{}", i + 4)).collect();
            let sql = format!(
                "INSERT OR IGNORE INTO readings (gateway, idx, ts_millis, {}) VALUES (?1, ?2, ?3, {})",
                names.join(", "),
                placeholders.join(", "),
            );

            let idx = row.idx as i64;
            let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(3 + row.values.len());
            params.push(&gateway);
            params.push(&idx);
            params.push(&row.ts_millis);
            for (_, value) in &row.values {
                params.push(value);
            }
            inserted += tx.execute(&sql, params.as_slice())?;
        }

        tx.execute(
            "INSERT INTO gateways (gateway, cursor, updated_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT(gateway) DO UPDATE SET cursor = excluded.cursor, updated_ms = excluded.updated_ms",
            rusqlite::params![gateway, new_cursor as i64, now_millis()],
        )?;

        tx.commit()?;
        Ok(Recorded {
            inserted,
            duplicates: rows.len() - inserted,
        })
    }

    /// The value column names of the `readings` table, in table order.
    #[cfg(test)]
    fn value_column_names(&self) -> Result<Vec<String>> {
        value_column_names(&self.conn)
    }

    /// Moves committed WAL frames into the base file, without blocking a
    /// concurrent reader or writer (it simply checkpoints as much as it can
    /// without waiting for one to finish). See the module doc comment for why
    /// an external reader needs this run often rather than left to WAL mode's
    /// own (much less frequent) automatic checkpointing.
    pub fn checkpoint(&self) -> Result<()> {
        self.conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);")?;
        Ok(())
    }
}

/// Whether a gateway's stored cursor should reset to `0` because its own
/// reported high-water mark (`latest_index`, from a `"data"` reply's
/// `payload.latest_index` — absent for a client too old to send it) has
/// fallen below what the server already believes it has pulled. This is what
/// happens when a gateway's local database is deleted and recreated: its
/// history shrinks back to (near) zero, and asking it for "everything after
/// N" for the old, now out-of-reach N would otherwise stall forever.
///
/// This is a pure comparison — no I/O — so `tunnel_session` can call it with
/// the in-memory `last_index` it is about to use for the *next* request,
/// before that request is even sent.
pub fn needs_reset(cursor: u64, latest_index: Option<u64>) -> bool {
    latest_index.is_some_and(|latest| latest < cursor)
}

/// Creates any column in `values` that `readings` does not already have, as
/// a nullable `REAL`. Existing rows read back `NULL` for a newly added
/// column — this table's schema only ever grows, never migrates or backfills
/// a fabricated value (see the module doc comment).
fn ensure_columns(conn: &Connection, values: &[(String, f64)]) -> Result<()> {
    let mut names: HashSet<String> = value_column_names(conn)?.into_iter().collect();
    for (name, _) in values {
        if names.contains(name) {
            continue;
        }
        validate_column_name(name)?;
        if names.len() >= MAX_VALUE_COLUMNS {
            bail!(
                "refusing to add column {name:?}: readings already has {MAX_VALUE_COLUMNS} value \
                 columns, the most this store allows"
            );
        }
        conn.execute(&format!("ALTER TABLE readings ADD COLUMN {name} REAL"), [])?;
        names.insert(name.clone());
    }
    Ok(())
}

/// The value column names of the `readings` table, in table order (excluding
/// the four bookkeeping columns every row has).
fn value_column_names(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("PRAGMA table_info(readings)")?;
    let mut names: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<_>>()?;
    names.retain(|name| !RESERVED_COLUMN_NAMES.contains(&name.as_str()));
    Ok(names)
}

/// Whether `name` can be used as a `readings` value column name. Reading
/// names arrive over the network, so `record_batch` enforces this itself
/// (via [`validate_column_name`]) before touching SQL — this is exposed so a
/// caller building a [`Row`] can drop a bad field from an otherwise-valid
/// measurement instead of losing the whole row (or the whole batch, since
/// `record_batch` is one transaction) to it.
pub fn is_valid_value_name(name: &str) -> bool {
    validate_column_name(name).is_ok()
}

/// Rejects a reading name that cannot be used as a SQL column name as-is, or
/// that collides with one of `readings`' own bookkeeping columns. Unlike the
/// client's identically named check on its own adapter-supplied literals,
/// this one runs against names that arrive over the network.
fn validate_column_name(name: &str) -> Result<()> {
    if RESERVED_COLUMN_NAMES.contains(&name) {
        bail!("reading name {name:?} collides with a reserved column name");
    }
    let starts_ok = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    let rest_ok = name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if starts_ok && rest_ok && name.len() <= 64 {
        Ok(())
    } else {
        bail!("invalid reading name for a column: {name:?}")
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(idx: u64, ts_millis: i64) -> Row {
        Row {
            idx,
            ts_millis,
            values: vec![
                ("voltage_l1_v".to_string(), 230.1),
                ("current_l1_a".to_string(), 1.83),
            ],
        }
    }

    #[test]
    fn cursor_is_zero_for_an_unknown_gateway() {
        let store = ServerStore::open_in_memory().unwrap();
        assert_eq!(store.cursor("pi-north").unwrap(), 0);
    }

    #[test]
    fn record_batch_persists_the_cursor() {
        let mut store = ServerStore::open_in_memory().unwrap();
        store.record_batch("pi-north", 3, &[row(1, 100), row(2, 200), row(3, 300)]).unwrap();
        assert_eq!(store.cursor("pi-north").unwrap(), 3);
    }

    #[test]
    fn cursor_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pqmeter.db");

        {
            let mut store = ServerStore::open(&path).unwrap();
            store.record_batch("pi-north", 5, &[row(5, 500)]).unwrap();
        }

        let reopened = ServerStore::open(&path).unwrap();
        assert_eq!(reopened.cursor("pi-north").unwrap(), 5);
    }

    #[test]
    fn gateways_have_independent_cursors() {
        let mut store = ServerStore::open_in_memory().unwrap();
        store.record_batch("pi-north", 2, &[row(1, 100), row(2, 200)]).unwrap();
        store.record_batch("pi-south", 9, &[row(9, 900)]).unwrap();

        assert_eq!(store.cursor("pi-north").unwrap(), 2);
        assert_eq!(store.cursor("pi-south").unwrap(), 9);
    }

    #[test]
    fn record_batch_reports_new_rows() {
        let mut store = ServerStore::open_in_memory().unwrap();
        let recorded = store
            .record_batch("pi-north", 3, &[row(1, 100), row(2, 200), row(3, 300)])
            .unwrap();
        assert_eq!(recorded, Recorded { inserted: 3, duplicates: 0 });
    }

    /// TODO item 4: re-delivering a batch the store already has a copy of
    /// (a reconnect re-pulling from a not-yet-persisted cursor, say) must not
    /// double the row count.
    #[test]
    fn re_recording_the_same_rows_is_a_no_op() {
        let mut store = ServerStore::open_in_memory().unwrap();
        store.record_batch("pi-north", 3, &[row(1, 100), row(2, 200), row(3, 300)]).unwrap();

        let recorded = store
            .record_batch("pi-north", 3, &[row(1, 100), row(2, 200), row(3, 300)])
            .unwrap();

        assert_eq!(recorded, Recorded { inserted: 0, duplicates: 3 });
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM readings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3);
    }

    /// A gateway whose local database was deleted and recreated restarts its
    /// own `idx` at 1 — those fresh rows must not collide with (and be
    /// dropped as duplicates of) the previous run's `idx` 1, since the
    /// timestamps differ.
    #[test]
    fn same_idx_with_a_different_timestamp_is_a_new_row() {
        let mut store = ServerStore::open_in_memory().unwrap();
        store.record_batch("pi-north", 1, &[row(1, 100)]).unwrap();

        let recorded = store.record_batch("pi-north", 1, &[row(1, 999_999)]).unwrap();

        assert_eq!(recorded, Recorded { inserted: 1, duplicates: 0 });
    }

    /// The whole point of nullable, additively-grown columns: a later batch
    /// naming a new value does not disturb earlier rows, which read back
    /// `NULL` for it, and a batch missing a value the table already has
    /// stores `NULL` for that row instead of failing.
    #[test]
    fn new_and_missing_columns_read_back_as_null() {
        let mut store = ServerStore::open_in_memory().unwrap();
        store.record_batch("pi-north", 1, &[row(1, 100)]).unwrap();
        store
            .record_batch(
                "pi-north",
                2,
                &[Row {
                    idx: 2,
                    ts_millis: 200,
                    values: vec![("frequency_hz".to_string(), 50.0)],
                }],
            )
            .unwrap();

        let mut names = store.value_column_names().unwrap();
        names.sort();
        assert_eq!(names, vec!["current_l1_a", "frequency_hz", "voltage_l1_v"]);

        let first_frequency: Option<f64> = store
            .conn
            .query_row("SELECT frequency_hz FROM readings WHERE idx = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(first_frequency, None);

        let second_voltage: Option<f64> = store
            .conn
            .query_row("SELECT voltage_l1_v FROM readings WHERE idx = 2", [], |r| r.get(0))
            .unwrap();
        assert_eq!(second_voltage, None);
    }

    #[test]
    fn invalid_reading_name_is_rejected() {
        let mut store = ServerStore::open_in_memory().unwrap();
        let err = store
            .record_batch(
                "pi-north",
                1,
                &[Row {
                    idx: 1,
                    ts_millis: 100,
                    values: vec![("bad name; DROP TABLE readings".to_string(), 1.0)],
                }],
            )
            .unwrap_err();
        assert!(err.to_string().contains("invalid reading name"));
    }

    #[test]
    fn reading_name_colliding_with_a_reserved_column_is_rejected() {
        let mut store = ServerStore::open_in_memory().unwrap();
        let err = store
            .record_batch(
                "pi-north",
                1,
                &[Row {
                    idx: 1,
                    ts_millis: 100,
                    values: vec![("gateway".to_string(), 1.0)],
                }],
            )
            .unwrap_err();
        assert!(err.to_string().contains("reserved column name"));
    }

    #[test]
    fn column_count_is_capped() {
        let mut store = ServerStore::open_in_memory().unwrap();
        let values: Vec<(String, f64)> = (0..MAX_VALUE_COLUMNS + 1)
            .map(|i| (format!("field_{i}"), i as f64))
            .collect();
        let err = store
            .record_batch("pi-north", 1, &[Row { idx: 1, ts_millis: 100, values }])
            .unwrap_err();
        assert!(err.to_string().contains("readings already has"));
    }

    #[test]
    fn needs_reset_is_true_only_when_the_gateways_history_shrank() {
        assert!(needs_reset(50, Some(1)));
        assert!(!needs_reset(50, Some(60)));
        assert!(!needs_reset(50, Some(50)));
        assert!(!needs_reset(50, None));
        assert!(!needs_reset(0, Some(0)));
    }
}
