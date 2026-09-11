//! Local persistence for meter readings.
//!
//! [`MeterStore`] keeps readings in a single SQLite file on the gateway (the Raspberry
//! Pi). Each call to [`MeterStore::insert`] (or [`MeterStore::insert_now`]) appends one
//! row; [`MeterStore::query`] and [`MeterStore::latest`] read them back. The file survives
//! reboots and can be copied off the Pi with `scp`, or inspected in place with
//! `sqlite3 pqmeter.db "SELECT * FROM readings ORDER BY ts_millis DESC LIMIT 10"`.
//!
//! [`Reading`] carries its values as an open list of `(name, value)` pairs rather than
//! fixed fields, mirroring `meter::MeterSnapshot`. The `readings` table's columns are not
//! fixed either: [`MeterStore::insert`] creates it on the very first call, with one `REAL`
//! column per name the caller passed in. Every later insert is expected to carry the same
//! names — in practice one gateway process uses one meter for the lifetime of a database
//! file, so this only needs to happen once. Mixing meter kinds into the same file is not
//! supported; point `--db` at a fresh file to switch.
//!
//! A consumer that pulls readings incrementally (for example a query endpoint the server
//! polls) keeps the largest [`StoredReading::id`] it has seen and passes it to
//! [`MeterStore::since_id`] on the next pull.
//!
//! The connection is not shared: each part of the program that needs the store opens its
//! own [`MeterStore`] on the same path. SQLite is opened in WAL mode so a writer and a
//! reader do not block each other, which is what lets a future query endpoint read the
//! file while the meter loop keeps writing.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use rusqlite::Connection;

/// One set of measured values, tagged with the moment it was taken.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    /// When the reading was taken, as Unix epoch milliseconds.
    pub ts_millis: i64,
    /// The measured values, in the order they should be stored and displayed.
    pub values: Vec<(String, f64)>,
}

impl Reading {
    /// The value of one named field, if this reading carries it.
    pub fn value(&self, name: &str) -> Option<f64> {
        self.values
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| *v)
    }
}

/// A reading as it is stored, carrying the database id assigned on insert.
///
/// The `id` is strictly increasing in insert order, gap-free in practice, and never
/// reused. It is the cursor for [`MeterStore::since_id`].
#[derive(Debug, Clone, PartialEq)]
pub struct StoredReading {
    /// Row id, assigned by SQLite when the reading was inserted.
    pub id: i64,
    /// The measured values.
    pub reading: Reading,
}

/// A handle to the SQLite file that stores meter readings.
pub struct MeterStore {
    conn: Connection,
}

impl MeterStore {
    /// Opens (creating if needed) the readings database at `path`.
    ///
    /// The `readings` table is not created here — its columns depend on the
    /// meter writing to it, so it is created by the first [`MeterStore::insert`]
    /// instead. Pointing this at a fresh path and at an existing database both
    /// work either way.
    pub fn open(path: &Path) -> Result<Self> {
        Self::from_connection(Connection::open(path)?)
    }

    /// Opens an in-memory database. For tests.
    #[cfg(test)]
    fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        // WAL lets a reader and the writer work at the same time; the busy timeout keeps a
        // brief overlap from surfacing as an error.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5_000)?;
        Ok(MeterStore { conn })
    }

    /// Appends one reading, creating the `readings` table first if this is the
    /// first insert this database file has ever seen.
    pub fn insert(&self, reading: &Reading) -> Result<()> {
        self.ensure_table(&reading.values)?;
        self.check_schema_matches(&reading.values)?;

        let columns: Vec<&str> = reading.values.iter().map(|(name, _)| name.as_str()).collect();
        let placeholders: Vec<String> = (0..columns.len()).map(|i| format!("?{}", i + 2)).collect();
        let sql = format!(
            "INSERT INTO readings (ts_millis, {}) VALUES (?1, {})",
            columns.join(", "),
            placeholders.join(", "),
        );

        let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(reading.values.len() + 1);
        params.push(&reading.ts_millis);
        for (_, value) in &reading.values {
            params.push(value);
        }
        self.conn.execute(&sql, params.as_slice())?;
        Ok(())
    }

    /// Appends one reading, stamped with the current system time.
    ///
    /// The `ts_millis` field of `reading` is ignored.
    pub fn insert_now(&self, mut reading: Reading) -> Result<()> {
        reading.ts_millis = now_millis();
        self.insert(&reading)
    }

    /// Returns every reading with `from_millis <= ts_millis <= to_millis`, oldest first.
    pub fn query(&self, from_millis: i64, to_millis: i64) -> Result<Vec<StoredReading>> {
        self.select(
            "WHERE ts_millis BETWEEN ?1 AND ?2 ORDER BY ts_millis ASC, id ASC",
            rusqlite::params![from_millis, to_millis],
        )
    }

    /// Returns the `n` most recent readings, newest first.
    pub fn latest(&self, n: usize) -> Result<Vec<StoredReading>> {
        self.select(
            "ORDER BY ts_millis DESC, id DESC LIMIT ?1",
            rusqlite::params![n as i64],
        )
    }

    /// Returns readings with `id` greater than `after_id`, oldest first, at most `limit`.
    ///
    /// Pass `after_id = 0` for the first pull, then the [`StoredReading::id`] of the last
    /// row returned. Unlike a time-range query this is immune to clock changes on the Pi
    /// and never returns the same row twice.
    pub fn since_id(&self, after_id: i64, limit: usize) -> Result<Vec<StoredReading>> {
        self.select(
            "WHERE id > ?1 ORDER BY id ASC LIMIT ?2",
            rusqlite::params![after_id, limit as i64],
        )
    }

    /// The largest [`StoredReading::id`] ever inserted, or `0` if nothing has
    /// been inserted (including when the `readings` table does not exist
    /// yet).
    ///
    /// This is the gateway's own high-water mark, reported alongside every
    /// data reply so the server can tell a shrunk history (this database file
    /// was deleted and recreated) from a normal pull — see
    /// `CONNECT_PROTOCOL.md`'s "Gateway Identity and History Resets" section.
    pub fn max_id(&self) -> Result<i64> {
        if self.value_column_names()?.is_none() {
            return Ok(0);
        }
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(id), 0) FROM readings", [], |row| row.get(0))?)
    }

    /// Runs one `SELECT ... {clause}` over every value column of the `readings`
    /// table, in its own column order, and maps each row back to a
    /// [`StoredReading`]. Returns no rows if the table does not exist yet
    /// (nothing has been inserted).
    fn select(&self, clause: &str, params: impl rusqlite::Params) -> Result<Vec<StoredReading>> {
        let Some(names) = self.value_column_names()? else {
            return Ok(Vec::new());
        };
        let sql = format!("SELECT id, ts_millis, {} FROM readings {clause}", names.join(", "));
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params, |row| row_to_stored(row, &names))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The value column names of the `readings` table, in table order, or
    /// `None` if the table does not exist yet.
    fn value_column_names(&self) -> Result<Option<Vec<String>>> {
        let mut stmt = self.conn.prepare("PRAGMA table_info(readings)")?;
        let mut names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<_>>()?;
        if names.is_empty() {
            return Ok(None);
        }
        names.retain(|name| name != "id" && name != "ts_millis");
        Ok(Some(names))
    }

    /// Creates the `readings` table with one `REAL` column per name in `values`,
    /// unless it already exists.
    fn ensure_table(&self, values: &[(String, f64)]) -> Result<()> {
        if self.value_column_names()?.is_some() {
            return Ok(());
        }
        for (name, _) in values {
            validate_column_name(name)?;
        }
        let columns = values
            .iter()
            .map(|(name, _)| format!("{name} REAL NOT NULL"))
            .collect::<Vec<_>>()
            .join(", ");
        self.conn.execute_batch(&format!(
            "CREATE TABLE readings (
                 id        INTEGER PRIMARY KEY AUTOINCREMENT,
                 ts_millis INTEGER NOT NULL,
                 {columns}
             );
             CREATE INDEX idx_readings_ts ON readings (ts_millis);"
        ))?;
        Ok(())
    }

    /// Fails with a message pointing at the fix if `values`' names don't
    /// exactly match the `readings` table's existing columns, instead of
    /// letting `insert` hit SQLite's own "no such column" / "NOT NULL
    /// constraint failed" errors. The table's schema is fixed for the life of
    /// a database file (see the module doc comment) — this is expected to
    /// trigger only when what a meter reports has changed since the file was
    /// first written.
    fn check_schema_matches(&self, values: &[(String, f64)]) -> Result<()> {
        let mut existing = self
            .value_column_names()?
            .expect("ensure_table just created the table if it did not already exist");
        let mut incoming: Vec<String> = values.iter().map(|(name, _)| name.clone()).collect();
        existing.sort();
        incoming.sort();
        if existing == incoming {
            return Ok(());
        }
        bail!(
            "this reading reports {incoming:?}, but the \"readings\" table already has columns \
             {existing:?} from an earlier run — a database file's schema is fixed for its \
             lifetime. Delete this --db file (or point --db at a new one) and restart if what \
             the meter reports has changed."
        )
    }
}

/// Rejects a reading name that cannot be used as a SQL column name as-is. Names
/// come from trusted adapter code (`&'static str` literals), not external
/// input, but a typo here should fail loudly rather than build broken SQL.
fn validate_column_name(name: &str) -> Result<()> {
    let starts_ok = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    let rest_ok = name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if starts_ok && rest_ok {
        Ok(())
    } else {
        bail!("invalid reading name for a column: {name:?}")
    }
}

fn row_to_stored(row: &rusqlite::Row<'_>, names: &[String]) -> rusqlite::Result<StoredReading> {
    let mut values = Vec::with_capacity(names.len());
    for (i, name) in names.iter().enumerate() {
        let value: f64 = row.get(2 + i)?;
        values.push((name.clone(), value));
    }
    Ok(StoredReading {
        id: row.get(0)?,
        reading: Reading {
            ts_millis: row.get(1)?,
            values,
        },
    })
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

    fn sample(ts_millis: i64) -> Reading {
        Reading {
            ts_millis,
            values: vec![
                ("voltage_l1_v".to_string(), 230.1),
                ("current_l1_a".to_string(), 1.83),
                ("active_power_l1_w".to_string(), 420.75),
                ("reactive_power_l1_var".to_string(), 12.5),
                ("phase_angle_l1_deg".to_string(), 3.2),
            ],
        }
    }

    fn timestamps(rows: &[StoredReading]) -> Vec<i64> {
        rows.iter().map(|r| r.reading.ts_millis).collect()
    }

    #[test]
    fn insert_then_query_returns_the_reading() {
        let store = MeterStore::open_in_memory().unwrap();
        let reading = sample(1_000);

        store.insert(&reading).unwrap();

        let got = store.query(0, 2_000).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].reading, reading);
    }

    #[test]
    fn query_excludes_readings_outside_the_range() {
        let store = MeterStore::open_in_memory().unwrap();
        for ts in [100, 200, 300] {
            store.insert(&sample(ts)).unwrap();
        }

        assert_eq!(timestamps(&store.query(150, 250).unwrap()), vec![200]);
    }

    #[test]
    fn query_returns_readings_oldest_first() {
        let store = MeterStore::open_in_memory().unwrap();
        for ts in [300, 100, 200] {
            store.insert(&sample(ts)).unwrap();
        }

        assert_eq!(
            timestamps(&store.query(0, 1_000).unwrap()),
            vec![100, 200, 300]
        );
    }

    #[test]
    fn latest_returns_the_newest_readings_first() {
        let store = MeterStore::open_in_memory().unwrap();
        for ts in [100, 200, 300] {
            store.insert(&sample(ts)).unwrap();
        }

        assert_eq!(timestamps(&store.latest(2).unwrap()), vec![300, 200]);
    }

    #[test]
    fn ids_are_assigned_in_insert_order() {
        let store = MeterStore::open_in_memory().unwrap();
        for ts in [100, 200, 300] {
            store.insert(&sample(ts)).unwrap();
        }

        let ids: Vec<i64> = store
            .query(0, 1_000)
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn since_id_returns_only_rows_after_the_cursor() {
        let store = MeterStore::open_in_memory().unwrap();
        for ts in [100, 200, 300] {
            store.insert(&sample(ts)).unwrap();
        }

        let all = store.since_id(0, 10).unwrap();
        assert_eq!(timestamps(&all), vec![100, 200, 300]);

        let after_second = store.since_id(all[1].id, 10).unwrap();
        assert_eq!(timestamps(&after_second), vec![300]);

        assert_eq!(store.since_id(all[2].id, 10).unwrap(), vec![]);
    }

    #[test]
    fn since_id_respects_the_limit() {
        let store = MeterStore::open_in_memory().unwrap();
        for ts in [10, 20, 30, 40, 50] {
            store.insert(&sample(ts)).unwrap();
        }

        assert_eq!(timestamps(&store.since_id(0, 2).unwrap()), vec![10, 20]);
    }

    #[test]
    fn insert_now_stamps_the_current_time() {
        let store = MeterStore::open_in_memory().unwrap();
        let before = now_millis();

        store.insert_now(sample(-1)).unwrap();

        let after = now_millis();
        let stored = store.query(0, i64::MAX).unwrap();
        assert_eq!(stored.len(), 1);
        let ts = stored[0].reading.ts_millis;
        assert!(
            (before..=after).contains(&ts),
            "ts {ts} not in [{before}, {after}]"
        );
    }

    #[test]
    fn readings_persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pqmeter.db");

        {
            let store = MeterStore::open(&path).unwrap();
            store.insert(&sample(42)).unwrap();
        }

        let reopened = MeterStore::open(&path).unwrap();
        let got = reopened.query(0, 1_000).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].reading, sample(42));
    }

    /// The `readings` table's columns follow whatever names the first insert
    /// carries, not a fixed schema — the whole point of the generic shape.
    #[test]
    fn insert_creates_columns_matching_the_first_readings_names() {
        let store = MeterStore::open_in_memory().unwrap();
        store
            .insert(&Reading {
                ts_millis: 1,
                values: vec![("frequency_hz".to_string(), 50.01), ("thd_pct".to_string(), 1.2)],
            })
            .unwrap();

        let rows = store.latest(1).unwrap();
        assert_eq!(rows[0].reading.value("frequency_hz"), Some(50.01));
        assert_eq!(rows[0].reading.value("thd_pct"), Some(1.2));
    }

    /// A reading whose names don't match an already-established schema fails
    /// with a message pointing at the fix, not a raw SQLite error.
    #[test]
    fn insert_with_mismatched_names_fails_with_an_actionable_message() {
        let store = MeterStore::open_in_memory().unwrap();
        store.insert(&sample(1)).unwrap();

        let err = store
            .insert(&Reading {
                ts_millis: 2,
                values: vec![("frequency_hz".to_string(), 50.0)],
            })
            .unwrap_err();

        let message = err.to_string();
        assert!(message.contains("--db"), "message was: {message}");
    }

    /// Reading with nothing inserted yet returns no rows instead of a SQL
    /// error about a missing table.
    #[test]
    fn reads_on_an_empty_store_return_no_rows() {
        let store = MeterStore::open_in_memory().unwrap();
        assert_eq!(store.latest(10).unwrap(), vec![]);
        assert_eq!(store.since_id(0, 10).unwrap(), vec![]);
        assert_eq!(store.query(0, i64::MAX).unwrap(), vec![]);
    }

    #[test]
    fn max_id_is_zero_on_an_empty_store() {
        let store = MeterStore::open_in_memory().unwrap();
        assert_eq!(store.max_id().unwrap(), 0);
    }

    #[test]
    fn max_id_tracks_the_newest_insert() {
        let store = MeterStore::open_in_memory().unwrap();
        for ts in [100, 200, 300] {
            store.insert(&sample(ts)).unwrap();
        }
        assert_eq!(store.max_id().unwrap(), 3);
    }

    fn now_millis() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }
}
