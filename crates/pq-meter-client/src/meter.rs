//! The meter data source: a [`MeterSource`] the gateway reads once per `"data"`
//! request, a [`ModbusMeter`] backed by the real UMG 605-PRO, and a
//! [`DummyMeter`] for tests.
//!
//! A real implementation holds a connected `umg605_modbus_client::Umg605ProClient`
//! and reads `voltage_l1`, `current_l1`, `power_l1_n`, `reactive_power_l1` and
//! `phase_angle_l1` (each a separate Modbus round-trip) into one [`MeterSnapshot`].
//! Boxing the trait here means wiring that in later is choosing which
//! `Box<dyn MeterSource>` `main` constructs, not restructuring the gateway.

use std::{
    net::SocketAddr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_modbus::Slave;
use umg605_modbus_client::Umg605ProClient;

/// One set of readings taken at one moment. Field names and units are the wire
/// contract described in `CONNECT_PROTOCOL.md`.
#[derive(Debug, Clone, Copy)]
pub struct MeterSnapshot {
    pub voltage_l1_v: f64,
    pub current_l1_a: f64,
    pub active_power_l1_w: f64,
    pub reactive_power_l1_var: f64,
    pub phase_angle_l1_deg: f64,
}

impl MeterSnapshot {
    /// Renders the snapshot as one entry of a `data` reply's `payload.data`
    /// array, stamped with the wire `index` and the time it was taken.
    pub fn to_json(self, index: u64) -> Value {
        json!({
            "timestamp": unix_timestamp_millis().to_string(),
            "voltage_l1_v": self.voltage_l1_v,
            "current_l1_a": self.current_l1_a,
            "active_power_l1_w": self.active_power_l1_w,
            "reactive_power_l1_var": self.reactive_power_l1_var,
            "phase_angle_l1_deg": self.phase_angle_l1_deg,
            "index": index,
        })
    }
}

/// A source of meter readings. Implementations may talk to real hardware, so
/// every read can fail; the caller turns a failure into the protocol's
/// `"error"` reply.
#[async_trait]
pub trait MeterSource: Send {
    /// Reads the meter as it is now. Called once per `"data"` request; the
    /// protocol's `payload.index` backfill range is not implemented, so every
    /// reply carries exactly one snapshot.
    async fn read_snapshot(&mut self) -> anyhow::Result<MeterSnapshot>;
}

/// A [`MeterSource`] that reads a UMG 605-PRO over Modbus TCP.
pub struct ModbusMeter {
    client: Umg605ProClient,
}

impl ModbusMeter {
    /// Connects to the meter at startup. The same connection is reused for
    /// each snapshot.
    pub async fn connect(
        socket_addr: SocketAddr,
        unit: u8,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        let client = Umg605ProClient::connect_tcp(socket_addr, Slave(unit), timeout).await?;
        Ok(Self { client })
    }
}

#[async_trait]
impl MeterSource for ModbusMeter {
    async fn read_snapshot(&mut self) -> anyhow::Result<MeterSnapshot> {
        Ok(MeterSnapshot {
            voltage_l1_v: self.client.voltage_l1().await? as f64,
            current_l1_a: self.client.current_l1().await? as f64,
            active_power_l1_w: self.client.power_l1_n().await? as f64,
            reactive_power_l1_var: self.client.reactive_power_l1().await? as f64,
            phase_angle_l1_deg: self.client.phase_angle_l1().await? as f64,
        })
    }
}

/// A stand-in for the real meter: plausible values that drift a little on
/// every read, so a running client is visibly alive without any hardware.
pub struct DummyMeter {
    reads: u64,
}

impl DummyMeter {
    pub fn new() -> Self {
        Self { reads: 0 }
    }
}

impl Default for DummyMeter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MeterSource for DummyMeter {
    async fn read_snapshot(&mut self) -> anyhow::Result<MeterSnapshot> {
        self.reads += 1;
        let seed = self.reads;
        Ok(MeterSnapshot {
            voltage_l1_v: 230.0 + jitter(seed, 2.0),
            current_l1_a: 5.0 + jitter(seed, 0.5),
            active_power_l1_w: 1150.0 + jitter(seed, 50.0),
            reactive_power_l1_var: 60.0 + jitter(seed, 10.0),
            phase_angle_l1_deg: 3.0 + jitter(seed, 1.0),
        })
    }
}

/// Deterministic pseudo-jitter so repeated dummy snapshots are not all identical.
fn jitter(seed: u64, amplitude: f64) -> f64 {
    let phase = seed as f64 * 0.7;
    amplitude * phase.sin()
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

    /// Two consecutive reads differ (the dummy is visibly "alive") and every
    /// value sits in a plausible range for its unit.
    #[tokio::test]
    async fn dummy_readings_drift_between_reads() {
        let mut meter = DummyMeter::new();
        let first = meter.read_snapshot().await.expect("first read");
        let second = meter.read_snapshot().await.expect("second read");

        assert_ne!(first.voltage_l1_v, second.voltage_l1_v);
        for snapshot in [first, second] {
            assert!((200.0..260.0).contains(&snapshot.voltage_l1_v));
            assert!((0.0..20.0).contains(&snapshot.current_l1_a));
            assert!((0.0..5000.0).contains(&snapshot.active_power_l1_w));
        }
    }

    /// `to_json` carries the five named readings, the requested index, and a
    /// timestamp that parses as a number.
    #[test]
    fn snapshot_json_has_wire_fields() {
        let snapshot = MeterSnapshot {
            voltage_l1_v: 231.5,
            current_l1_a: 5.2,
            active_power_l1_w: 1200.0,
            reactive_power_l1_var: 65.0,
            phase_angle_l1_deg: 3.1,
        };
        let value = snapshot.to_json(7);

        assert_eq!(value["voltage_l1_v"], 231.5);
        assert_eq!(value["current_l1_a"], 5.2);
        assert_eq!(value["active_power_l1_w"], 1200.0);
        assert_eq!(value["reactive_power_l1_var"], 65.0);
        assert_eq!(value["phase_angle_l1_deg"], 3.1);
        assert_eq!(value["index"], 7);
        let timestamp = value["timestamp"].as_str().expect("timestamp is a string");
        timestamp
            .parse::<u128>()
            .expect("timestamp parses as a number");
    }
}
