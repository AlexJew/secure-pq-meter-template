//! Local persistence for meter readings.
//!
//! [`MeterStore`] keeps readings in a single SQLite file on the gateway (the Raspberry
//! Pi). Each call to [`MeterStore::insert`] (or [`MeterStore::insert_now`]) appends one
//! row; [`MeterStore::query`] and [`MeterStore::latest`] read them back. The file survives
//! reboots and can be copied off the Pi with `scp`, or inspected in place with
//! `sqlite3 pqmeter.db "SELECT * FROM readings ORDER BY ts_millis DESC LIMIT 10"`.
//!
//! Typical use from the meter loop:
//!
//! ```ignore
//! let store = MeterStore::open(Path::new("pqmeter.db"))?;
//! loop {
//!     let reading = Reading {
//!         ts_millis: 0, // overwritten by insert_now
//!         voltage_l1: client.voltage_l1().await? as f64,
//!         current_l1: client.current_l1().await? as f64,
//!         real_power_l1: client.power_l1_n().await? as f64,
//!         reactive_power_l1: client.reactive_power_l1().await? as f64,
//!         phase_angle_l1: client.phase_angle_l1().await? as f64,
//!     };
//!     store.insert_now(reading)?;
//! }
//! ```
//!
//! A consumer that pulls readings incrementally (for example a query endpoint the server
//! polls) keeps the largest [`StoredReading::id`] it has seen and passes it to
//! [`MeterStore::since_id`] on the next pull:
//!
//! ```ignore
//! let mut cursor = 0;
//! loop {
//!     let batch = store.since_id(cursor, 1_000)?;
//!     if let Some(last) = batch.last() {
//!         cursor = last.id;
//!         send(&batch)?;
//!     }
//! }
//! ```
//!
//! The connection is not shared: each part of the program that needs the store opens its
//! own [`MeterStore`] on the same path. SQLite is opened in WAL mode so a writer and a
//! reader do not block each other, which is what lets a future query endpoint read the
//! file while the meter loop keeps writing.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use rusqlite::Connection;

/// One set of measured values, tagged with the moment it was taken.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    /// When the reading was taken, as Unix epoch milliseconds.
    pub ts_millis: i64,
    /// Voltage of phase L1 to neutral, in volts.
    pub voltage_l1: f64,
    /// Current of phase L1, in amperes.
    pub current_l1: f64,
    /// Active power of phase L1 to neutral, in watts.
    pub real_power_l1: f64,
    /// Reactive power of phase L1, in vars.
    pub reactive_power_l1: f64,
    /// Phase angle between voltage and current of phase L1, in degrees.
    pub phase_angle_l1: f64,
}

/// A reading as it is stored, carrying the database id assigned on insert.
///
/// The `id` is strictly increasing in insert order, gap-free in practice, and never
/// reused. It is the cursor for [`MeterStore::since_id`].
#[derive(Debug, Clone, Copy, PartialEq)]
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
    /// The `readings` table and its index are created on first use, so pointing this at a
    /// fresh path and at an existing database both work.
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
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS readings (
                 id                INTEGER PRIMARY KEY AUTOINCREMENT,
                 ts_millis         INTEGER NOT NULL,
                 voltage_l1        REAL NOT NULL,
                 current_l1        REAL NOT NULL,
                 real_power_l1     REAL NOT NULL,
                 reactive_power_l1 REAL NOT NULL,
                 phase_angle_l1    REAL NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_readings_ts ON readings (ts_millis);",
        )?;
        Ok(MeterStore { conn })
    }

    /// Appends one reading.
    pub fn insert(&self, reading: &Reading) -> Result<()> {
        self.conn.execute(
            "INSERT INTO readings
                 (ts_millis, voltage_l1, current_l1, real_power_l1, reactive_power_l1, phase_angle_l1)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                reading.ts_millis,
                reading.voltage_l1,
                reading.current_l1,
                reading.real_power_l1,
                reading.reactive_power_l1,
                reading.phase_angle_l1,
            ],
        )?;
        Ok(())
    }

    /// Appends one reading, stamped with the current system time.
    ///
    /// The `ts_millis` field of `reading` is ignored.
    pub fn insert_now(&self, mut reading: Reading) -> Result<()> {
        reading.ts_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        self.insert(&reading)
    }

    /// Returns every reading with `from_millis <= ts_millis <= to_millis`, oldest first.
    pub fn query(&self, from_millis: i64, to_millis: i64) -> Result<Vec<StoredReading>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts_millis, voltage_l1, current_l1, real_power_l1, reactive_power_l1, phase_angle_l1
             FROM readings
             WHERE ts_millis BETWEEN ?1 AND ?2
             ORDER BY ts_millis ASC, id ASC",
        )?;
        let rows = stmt.query_map(rusqlite::params![from_millis, to_millis], row_to_stored)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Returns the `n` most recent readings, newest first.
    pub fn latest(&self, n: usize) -> Result<Vec<StoredReading>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts_millis, voltage_l1, current_l1, real_power_l1, reactive_power_l1, phase_angle_l1
             FROM readings
             ORDER BY ts_millis DESC, id DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![n as i64], row_to_stored)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Returns readings with `id` greater than `after_id`, oldest first, at most `limit`.
    ///
    /// Pass `after_id = 0` for the first pull, then the [`StoredReading::id`] of the last
    /// row returned. Unlike a time-range query this is immune to clock changes on the Pi
    /// and never returns the same row twice.
    pub fn since_id(&self, after_id: i64, limit: usize) -> Result<Vec<StoredReading>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts_millis, voltage_l1, current_l1, real_power_l1, reactive_power_l1, phase_angle_l1
             FROM readings
             WHERE id > ?1
             ORDER BY id ASC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![after_id, limit as i64], row_to_stored)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

fn row_to_stored(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredReading> {
    Ok(StoredReading {
        id: row.get(0)?,
        reading: Reading {
            ts_millis: row.get(1)?,
            voltage_l1: row.get(2)?,
            current_l1: row.get(3)?,
            real_power_l1: row.get(4)?,
            reactive_power_l1: row.get(5)?,
            phase_angle_l1: row.get(6)?,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ts_millis: i64) -> Reading {
        Reading {
            ts_millis,
            voltage_l1: 230.1,
            current_l1: 1.83,
            real_power_l1: 420.75,
            reactive_power_l1: 12.5,
            phase_angle_l1: 3.2,
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

        let ids: Vec<i64> = store.query(0, 1_000).unwrap().iter().map(|r| r.id).collect();
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

    fn now_millis() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }
}
