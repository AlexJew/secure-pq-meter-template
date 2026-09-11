//! [`ModbusMeter`]: the [`super::MeterSource`] adapter for the UMG 605-PRO,
//! backed by a connected `umg605_modbus_client::Umg605ProClient`. This is the
//! only module in `pq-meter-client` that knows about `umg605_modbus_client`; a
//! second vendor's meter would be a sibling module, not a change here.

use std::{net::SocketAddr, time::Duration};

use async_trait::async_trait;
use tokio_modbus::Slave;
use umg605_modbus_client::Umg605ProClient;

use super::{MeterSnapshot, MeterSource, Reading};

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
        Ok(MeterSnapshot {
            readings: vec![
                Reading {
                    name: "voltage_l1_v",
                    value: self.client.voltage_l1().await? as f64,
                },
                Reading {
                    name: "current_l1_a",
                    value: self.client.current_l1().await? as f64,
                },
                Reading {
                    name: "active_power_l1_w",
                    value: self.client.power_l1_n().await? as f64,
                },
                Reading {
                    name: "reactive_power_l1_var",
                    value: self.client.reactive_power_l1().await? as f64,
                },
                Reading {
                    name: "phase_angle_l1_deg",
                    value: self.client.phase_angle_l1().await? as f64,
                },
            ],
        })
    }
}
