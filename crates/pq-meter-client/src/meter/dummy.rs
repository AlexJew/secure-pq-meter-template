//! [`DummyMeter`]: a [`super::MeterSource`] stand-in for the real meter, with
//! plausible values that drift a little on every read, so a running client is
//! visibly alive without any hardware. Used for demos and tests, alongside
//! the real [`super::umg605::ModbusMeter`] — a second vendor's meter would be
//! another sibling module implementing the same trait, not a change here.

use async_trait::async_trait;

use super::{MeterSnapshot, MeterSource, Reading};

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
    fn kind(&self) -> &'static str {
        "dummy"
    }

    async fn read_snapshot(&mut self) -> anyhow::Result<MeterSnapshot> {
        self.reads += 1;
        let seed = self.reads;
        Ok(MeterSnapshot {
            readings: vec![
                Reading {
                    name: "voltage_l1_v",
                    value: 230.0 + jitter(seed, 2.0),
                },
                Reading {
                    name: "current_l1_a",
                    value: 5.0 + jitter(seed, 0.5),
                },
                Reading {
                    name: "active_power_l1_w",
                    value: 1150.0 + jitter(seed, 50.0),
                },
                Reading {
                    name: "reactive_power_l1_var",
                    value: 60.0 + jitter(seed, 10.0),
                },
                Reading {
                    name: "phase_angle_l1_deg",
                    value: 3.0 + jitter(seed, 1.0),
                },
            ],
        })
    }
}

/// Deterministic pseudo-jitter so repeated dummy snapshots are not all identical.
fn jitter(seed: u64, amplitude: f64) -> f64 {
    let phase = seed as f64 * 0.7;
    amplitude * phase.sin()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(snapshot: &MeterSnapshot, name: &str) -> f64 {
        snapshot
            .readings
            .iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("no reading named {name}"))
            .value
    }

    /// Two consecutive reads differ (the dummy is visibly "alive") and every
    /// value sits in a plausible range for its unit.
    #[tokio::test]
    async fn dummy_readings_drift_between_reads() {
        let mut meter = DummyMeter::new();
        let first = meter.read_snapshot().await.expect("first read");
        let second = meter.read_snapshot().await.expect("second read");

        assert_ne!(value(&first, "voltage_l1_v"), value(&second, "voltage_l1_v"));
        for snapshot in [&first, &second] {
            assert!((200.0..260.0).contains(&value(snapshot, "voltage_l1_v")));
            assert!((0.0..20.0).contains(&value(snapshot, "current_l1_a")));
            assert!((0.0..5000.0).contains(&value(snapshot, "active_power_l1_w")));
        }
    }
}
