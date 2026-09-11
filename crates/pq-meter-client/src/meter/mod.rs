//! The meter data source: a [`MeterSource`] the gateway reads once per `"data"`
//! request.
//!
//! A [`MeterSnapshot`] is an open, ordered list of named [`Reading`]s rather than
//! a fixed set of fields, so a `MeterSource` implementation can report whatever
//! quantities its device has without this type, [`crate::storage`], or the wire
//! protocol changing. Two implementations live as sibling modules: `umg605`
//! (see [`umg605::ModbusMeter`]) for the real UMG 605-PRO, and `dummy` (see
//! [`dummy::DummyMeter`]) for demos and tests with no hardware attached. A
//! third vendor would be another sibling module implementing the same trait,
//! not a change here.

pub mod dummy;
pub mod umg605;

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::{Value, json};

/// One measured quantity. `name` is the wire field name; per the convention in
/// `CONNECT_PROTOCOL.md`, the unit is already encoded in it (e.g.
/// `"voltage_l1_v"`), so no separate unit type is needed here.
#[derive(Debug, Clone, Copy)]
pub struct Reading {
    pub name: &'static str,
    pub value: f64,
}

/// Whatever one [`MeterSource`] measured in one pass.
#[derive(Debug, Clone)]
pub struct MeterSnapshot {
    pub readings: Vec<Reading>,
}

impl MeterSnapshot {
    /// Renders the snapshot as one entry of a `data` reply's `payload.data`
    /// array, stamped with the wire `index` and the time it was taken.
    pub fn to_json(&self, index: u64) -> Value {
        let mut object = serde_json::Map::with_capacity(self.readings.len() + 2);
        object.insert(
            "timestamp".to_string(),
            json!(unix_timestamp_millis().to_string()),
        );
        for reading in &self.readings {
            object.insert(reading.name.to_string(), json!(reading.value));
        }
        object.insert("index".to_string(), json!(index));
        Value::Object(object)
    }
}

/// A source of meter readings. Implementations may talk to real hardware, so
/// every read can fail; the caller turns a failure into the protocol's
/// `"error"` reply.
#[async_trait]
pub trait MeterSource: Send {
    /// A short, stable identifier for what produced readings (e.g.
    /// `"umg605-pro"`, `"dummy"`) — used in logs, and to tell meter types apart
    /// once there is more than one.
    fn kind(&self) -> &'static str;

    /// Reads the meter as it is now. Called once per `"data"` request; the
    /// protocol's `payload.index` backfill range is not implemented, so every
    /// reply carries exactly one snapshot.
    async fn read_snapshot(&mut self) -> anyhow::Result<MeterSnapshot>;
}

/// The current time as milliseconds since the Unix epoch.
fn unix_timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `to_json` carries every reading by name, the requested index, and a
    /// timestamp that parses as a number.
    #[test]
    fn snapshot_json_has_wire_fields() {
        let snapshot = MeterSnapshot {
            readings: vec![
                Reading { name: "voltage_l1_v", value: 231.5 },
                Reading { name: "current_l1_a", value: 5.2 },
            ],
        };
        let value = snapshot.to_json(7);

        assert_eq!(value["voltage_l1_v"], 231.5);
        assert_eq!(value["current_l1_a"], 5.2);
        assert_eq!(value["index"], 7);
        let timestamp = value["timestamp"].as_str().expect("timestamp is a string");
        timestamp
            .parse::<u128>()
            .expect("timestamp parses as a number");
    }
}
