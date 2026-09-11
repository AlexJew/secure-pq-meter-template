//! [`ModbusMeter`]: the [`super::MeterSource`] adapter for the UMG 605-PRO,
//! backed by a connected `umg605_modbus_client::Umg605ProClient`. This is the
//! only module in `pq-meter-client` that knows about `umg605_modbus_client`; a
//! second vendor's meter would be a sibling module, not a change here.

use std::{net::SocketAddr, time::Duration};

use async_trait::async_trait;
use tokio_modbus::Slave;
use umg605_modbus_client::Umg605ProClient;

use super::{MeterSnapshot, MeterSource, Reading, push_harmonic_array};

/// Harmonics captured per array: indices `0..HARMONIC_COUNT`, i.e. the 1st
/// through 25th harmonic. Matches the harmonic-oscilloscope design doc's
/// starting point — enough for typical household SMPS harmonic content,
/// while keeping each array read (50 registers at this count) well under
/// Modbus's 125-register-per-request limit.
const HARMONIC_COUNT: u16 = 25;

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
    fn kind(&self) -> &'static str {
        "umg605-pro"
    }

    async fn read_snapshot(&mut self) -> anyhow::Result<MeterSnapshot> {
        let mut readings = vec![
            Reading {
                name: "voltage_l1_v".to_string(),
                value: self.client.voltage_l1().await? as f64,
            },
            Reading {
                name: "current_l1_a".to_string(),
                value: self.client.current_l1().await? as f64,
            },
            Reading {
                name: "active_power_l1_w".to_string(),
                value: self.client.power_l1_n().await? as f64,
            },
            Reading {
                name: "reactive_power_l1_var".to_string(),
                value: self.client.reactive_power_l1().await? as f64,
            },
            Reading {
                name: "phase_angle_l1_deg".to_string(),
                value: self.client.phase_angle_l1().await? as f64,
            },
            Reading {
                name: "power_factor_l1".to_string(),
                value: self.client.power_factor_l1().await? as f64,
            },
            Reading {
                name: "current_thd_l1_pct".to_string(),
                value: self.client.current_thd_l1().await? as f64,
            },
            Reading {
                name: "current_peak_positive_l1_a".to_string(),
                value: self.client.current_peak_positive_l1().await? as f64,
            },
            Reading {
                name: "current_peak_negative_l1_a".to_string(),
                value: self.client.current_peak_negative_l1().await? as f64,
            },
            Reading {
                name: "current_crest_factor_l1".to_string(),
                value: self.client.current_crest_factor_l1().await? as f64,
            },
            Reading {
                name: "power_factor_sign_l1".to_string(),
                value: self.client.power_factor_sign_l1().await? as f64,
            },
            Reading {
                name: "power_factor_fundamental_l1".to_string(),
                value: self.client.power_factor_fundamental_l1().await? as f64,
            },
        ];

        push_harmonic_array(
            &mut readings,
            "voltage_harmonic_mag_l1",
            "v",
            self.client.voltage_harmonics_l1(HARMONIC_COUNT).await?,
        );
        push_harmonic_array(
            &mut readings,
            "voltage_harmonic_phase_l1",
            "deg",
            self.client.voltage_phase_harmonics_l1(HARMONIC_COUNT).await?,
        );
        push_harmonic_array(
            &mut readings,
            "current_harmonic_mag_l1",
            "a",
            self.client.current_harmonics_l1(HARMONIC_COUNT).await?,
        );
        push_harmonic_array(
            &mut readings,
            "current_harmonic_phase_l1",
            "deg",
            self.client.current_phase_harmonics_l1(HARMONIC_COUNT).await?,
        );

        Ok(MeterSnapshot { readings })
    }
}
