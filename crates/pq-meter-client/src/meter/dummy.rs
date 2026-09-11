//! [`DummyMeter`]: a [`super::MeterSource`] stand-in for the real meter, with
//! plausible values that drift a little on every read, so a running client is
//! visibly alive without any hardware. Used for demos and tests, alongside
//! the real [`super::umg605::ModbusMeter`] — a second vendor's meter would be
//! another sibling module implementing the same trait, not a change here.

use async_trait::async_trait;

use super::{MeterSnapshot, MeterSource, Reading, push_harmonic_array};

/// Harmonics faked per array. Matches `umg605::HARMONIC_COUNT` in spirit
/// (same field shape as the real meter), but is not required to track it
/// exactly — this is a stand-in, not a mirror.
const HARMONIC_COUNT: usize = 25;

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

        let mut readings = vec![
            Reading {
                name: "voltage_l1_v".to_string(),
                value: 230.0 + jitter(seed, 2.0),
            },
            Reading {
                name: "current_l1_a".to_string(),
                value: 5.0 + jitter(seed, 0.5),
            },
            Reading {
                name: "active_power_l1_w".to_string(),
                value: 1150.0 + jitter(seed, 50.0),
            },
            Reading {
                name: "reactive_power_l1_var".to_string(),
                value: 60.0 + jitter(seed, 10.0),
            },
            Reading {
                name: "phase_angle_l1_deg".to_string(),
                value: 3.0 + jitter(seed, 1.0),
            },
            Reading {
                name: "power_factor_l1".to_string(),
                value: 0.95 + jitter(seed, 0.03),
            },
            Reading {
                name: "current_thd_l1_pct".to_string(),
                value: 8.0 + jitter(seed, 3.0).abs(),
            },
            Reading {
                name: "current_peak_positive_l1_a".to_string(),
                value: 7.5 + jitter(seed, 1.0).abs(),
            },
            Reading {
                name: "current_peak_negative_l1_a".to_string(),
                value: -7.3 - jitter(seed, 1.0).abs(),
            },
            Reading {
                name: "current_crest_factor_l1".to_string(),
                value: 1.6 + jitter(seed, 0.2).abs(),
            },
            Reading {
                name: "power_factor_sign_l1".to_string(),
                value: if seed.is_multiple_of(5) { -1.0 } else { 1.0 },
            },
            Reading {
                name: "power_factor_fundamental_l1".to_string(),
                value: 0.97 + jitter(seed, 0.02),
            },
        ];

        push_harmonic_array(
            &mut readings,
            "voltage_harmonic_mag_l1",
            "v",
            dummy_harmonic_magnitudes(seed, 230.0, HARMONIC_COUNT),
        );
        push_harmonic_array(
            &mut readings,
            "voltage_harmonic_phase_l1",
            "deg",
            dummy_harmonic_phases(seed, HARMONIC_COUNT),
        );
        push_harmonic_array(
            &mut readings,
            "current_harmonic_mag_l1",
            "a",
            dummy_harmonic_magnitudes(seed, 5.0, HARMONIC_COUNT),
        );
        push_harmonic_array(
            &mut readings,
            "current_harmonic_phase_l1",
            "deg",
            dummy_harmonic_phases(seed, HARMONIC_COUNT),
        );

        Ok(MeterSnapshot { readings })
    }
}

/// Deterministic pseudo-jitter so repeated dummy snapshots are not all identical.
fn jitter(seed: u64, amplitude: f64) -> f64 {
    let phase = seed as f64 * 0.7;
    amplitude * phase.sin()
}

/// Plausible per-harmonic magnitudes for a decaying, odd-harmonic-heavy
/// spectrum — typical of a non-PFC SMPS load (phone/laptop charger) — so the
/// dummy meter's harmonic arrays look like something worth fingerprinting
/// rather than flat noise. `fundamental` is the order-1 (`h1`) magnitude.
fn dummy_harmonic_magnitudes(seed: u64, fundamental: f64, count: usize) -> Vec<f32> {
    (1..=count)
        .map(|order| {
            let magnitude = if order == 1 {
                fundamental
            } else if order % 2 == 1 {
                // Odd harmonics decay but stay visible, as in a rectifier-style load.
                fundamental * 0.35 / (order as f64).powf(1.2)
            } else {
                // Even harmonics are near-negligible for a roughly symmetric waveform.
                fundamental * 0.02 / order as f64
            };
            (magnitude + jitter(seed + order as u64, magnitude * 0.1).abs()) as f32
        })
        .collect()
}

/// Plausible per-harmonic phases in degrees, drifting a little between reads.
fn dummy_harmonic_phases(seed: u64, count: usize) -> Vec<f32> {
    (1..=count)
        .map(|order| jitter(seed + order as u64 * 7, 90.0) as f32)
        .collect()
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
            assert!((0.0..=1.0).contains(&value(snapshot, "power_factor_l1")));
            assert!((0.0..100.0).contains(&value(snapshot, "current_thd_l1_pct")));
            assert!((0.0..20.0).contains(&value(snapshot, "current_peak_positive_l1_a")));
            assert!((-20.0..0.0).contains(&value(snapshot, "current_peak_negative_l1_a")));
            assert!((1.0..5.0).contains(&value(snapshot, "current_crest_factor_l1")));
            assert!([-1.0, 1.0].contains(&value(snapshot, "power_factor_sign_l1")));
            assert!((0.0..=1.0).contains(&value(snapshot, "power_factor_fundamental_l1")));

            // Harmonic arrays: right count, fundamental dominates, phases stay in range.
            for base in ["voltage_harmonic_mag_l1", "current_harmonic_mag_l1"] {
                for order in 1..=HARMONIC_COUNT {
                    let name = format!("{base}_h{order}_{}", if base.starts_with('v') { "v" } else { "a" });
                    assert!(
                        value(snapshot, &name) >= 0.0,
                        "{name} should be non-negative"
                    );
                }
            }
            assert!(value(snapshot, "voltage_harmonic_mag_l1_h1_v") > value(snapshot, "voltage_harmonic_mag_l1_h3_v"));
            assert!(value(snapshot, "current_harmonic_mag_l1_h1_a") > value(snapshot, "current_harmonic_mag_l1_h3_a"));
            for base in ["voltage_harmonic_phase_l1", "current_harmonic_phase_l1"] {
                for order in 1..=HARMONIC_COUNT {
                    let name = format!("{base}_h{order}_deg");
                    assert!((-180.0..=180.0).contains(&value(snapshot, &name)));
                }
            }
        }
    }
}
