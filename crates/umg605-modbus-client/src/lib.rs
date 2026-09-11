//! A Basic Modbus client for the Umg605Pro device.
//! 
//! See [Modbus register map] for the list of registers that can be read from the device.
//! 
//! For more information about the Umg605Pro device, see the [official documentation](https://www.janitza.com/en/products/umg-605-pro/downloads).
//! 
//! [Modbus register map]: https://assets.janitza.com/ce18jq9ih0x6/b83ae2356a42a682591109/ef2bc2b24a6b7c77de4dbda20e43cebf/janitza-mal-umg605pro-en.pdf

use std::borrow::Cow;
use std::net::{SocketAddr};
use std::time::{Duration};

use tokio_modbus::client::Reader;
use tokio_modbus::Slave;

/// Default Modbus TCP port
pub const DEFAULT_MODBUS_PORT: u16 = 502;

/// TCP Modbus client for the Umg605Pro device.
pub struct Umg605ProClient {
    client: tokio_modbus::client::Context,
    timeout: Duration,
}

/// Errors that can occur when connecting to the Umg605Pro device.
#[derive(thiserror::Error, Debug)]
pub enum ConnectError {
    #[error("connection to {0} timed out after {1:?}")]
    Timeout(SocketAddr, Duration),
    #[error("failed to connect: {0}")]
    Connect(std::io::Error),
}

impl Umg605ProClient {
    /// Creates a new instance of the Umg605ProClient over TCP Modbus.
    /// 
    /// ### Parameters
    /// * `socket_addr` is the IP address and port of the Umg605Pro device.
    /// * `unit` is the Modbus unit id configured on the meter. For TCP Modbus, this can usually be set to 1.
    /// * `timeout` is the timeout for connecting and for each register read.
    pub async fn connect_tcp(
        socket_addr: SocketAddr,
        unit: Slave,
        timeout: Duration,
    ) -> Result<Self, ConnectError> {
        let modbus_context = tokio::time::timeout(
            timeout,
            tokio_modbus::client::tcp::connect_slave(socket_addr, unit),
        )
        .await
            .map_err(|_| ConnectError::Timeout(socket_addr, timeout))?
            .map_err(ConnectError::Connect)?;

        Ok(Umg605ProClient {
            client: modbus_context,
            timeout,
        })
    }
}


#[derive(thiserror::Error, Debug)]
pub enum ReadError {
    #[error("read timed out after {0:?}")]
    Timeout(Duration),
    #[error("transport error: {0}")]
    Transport(#[from] std::io::Error),
    #[error("protocol error: {0}")]
    Protocol(#[from] tokio_modbus::ProtocolError),
    #[error("modbus exception: {0}")]
    ModbusException(#[from] tokio_modbus::ExceptionCode),
    #[error("decode error: {0}")]
    DecodeError(Cow<'static, str>),
}

impl Umg605ProClient {

    /// Reads `count` holding registers, returning a vector of `u16` values.
    pub async fn read_holding_registers(&mut self, addr: u16, count: u16) -> Result<Vec<u16>, ReadError> {
        tokio::time::timeout(self.timeout, self.client.read_holding_registers(addr, count))
            .await
            .map_err(|_| ReadError::Timeout(self.timeout))?
            .map_err(|e| match e {
                tokio_modbus::Error::Protocol(protocol_error) => ReadError::Protocol(protocol_error),
                tokio_modbus::Error::Transport(error) => ReadError::Transport(error),
            })?
            .map_err(ReadError::ModbusException)
    }


    /// Reads a float32 value spanning the two holding registers starting at `addr`.
    pub async fn read_f32(&mut self, addr: u16) -> Result<f32, ReadError> {
        let regs = self.read_holding_registers(addr, 2).await?;
        let [hi, lo] = regs[..] else {
            return Err(ReadError::DecodeError(Cow::Owned(format!(
                "expected 2 registers at {addr}, got {}",
                regs.len()
            ))));
        };
        Ok(f32::from_bits(((hi as u32) << 16) | (lo as u32)))
    }

    /// Reads `count` consecutive float32 values starting at `addr` (a `float`
    /// array in the datasheet's sense, e.g. `_FFT_IL1[0..count]`), in a single
    /// Modbus request.
    ///
    /// Modbus limits a single read to 125 registers, i.e. 62 floats; fails
    /// without going to the wire if `count` would exceed that, rather than
    /// letting the server reject an oversized request.
    pub async fn read_f32_array(&mut self, addr: u16, count: u16) -> Result<Vec<f32>, ReadError> {
        const MAX_FLOATS_PER_REQUEST: u16 = 62;
        if count > MAX_FLOATS_PER_REQUEST {
            return Err(ReadError::DecodeError(Cow::Owned(format!(
                "requested {count} floats ({} registers) at {addr}, but Modbus allows at most \
                 125 registers ({MAX_FLOATS_PER_REQUEST} floats) per read",
                count as u32 * 2,
            ))));
        }
        let regs = self.read_holding_registers(addr, count * 2).await?;
        Ok(regs
            .chunks_exact(2)
            .map(|pair| f32::from_bits(((pair[0] as u32) << 16) | (pair[1] as u32)))
            .collect())
    }
}



// Register reading functions for the Umg605Pro device.
impl Umg605ProClient {

    /// Fetches the voltage of phase L1 in volts.
    pub async fn voltage_l1(&mut self) -> Result<f32, ReadError> {
        self.read_f32(19000).await
    }

    /// Fetches the current of phase L1 in amperes.
    pub async fn current_l1(&mut self) -> Result<f32, ReadError> {
        self.read_f32(19012).await
    }

    /// Fetches the active power of phase L1 to neutral in watts.
    pub async fn power_l1_n(&mut self) -> Result<f32, ReadError> {
        self.read_f32(19020).await
    }

    /// Fetches the reactive power of phase L1 in vars.
    pub async fn reactive_power_l1(&mut self) -> Result<f32, ReadError> {
        self.read_f32(19036).await
    }

    /// Fetches the phase angle between voltage and current of phase L1 in degrees.
    ///
    /// The UMG 605-PRO does not expose a mean-value register for phase angle.
    pub async fn phase_angle_l1(&mut self) -> Result<f32, ReadError> {
        self.read_f32(3971).await
    }

    /// Fetches the (vectorial) power factor of phase L1, `_PFLN[0]`.
    pub async fn power_factor_l1(&mut self) -> Result<f32, ReadError> {
        self.read_f32(3893).await
    }

    /// Fetches the total harmonic distortion of the current of phase L1 in
    /// percent, `_THD_IL[0]`.
    pub async fn current_thd_l1(&mut self) -> Result<f32, ReadError> {
        self.read_f32(3813).await
    }

    /// Fetches the highest positive sampling value of the current of phase L1
    /// from the last 200ms measuring window, in amperes, `_IL_POS_PEAK[0]`.
    pub async fn current_peak_positive_l1(&mut self) -> Result<f32, ReadError> {
        self.read_f32(4059).await
    }

    /// Fetches the voltage L-N harmonic magnitudes of phase L1, in volts,
    /// `_FFT_UL1[0..count]` — index 0 is the fundamental (1st harmonic).
    pub async fn voltage_harmonics_l1(&mut self, count: u16) -> Result<Vec<f32>, ReadError> {
        self.read_f32_array(391, count).await
    }

    /// Fetches the voltage L-N harmonic phases of phase L1, `_FFT_ULZ1[0..count]`.
    ///
    /// The datasheet's unit column lists "V" for this register, matching its
    /// magnitude sibling — almost certainly a documentation error for what is
    /// most likely a phase angle in degrees, but this has not been confirmed
    /// against the real meter. Treat the unit as provisional until validated
    /// (see `harmonic-oscilloscope-implementation.md` section 5.3, Checks A-D).
    pub async fn voltage_phase_harmonics_l1(&mut self, count: u16) -> Result<Vec<f32>, ReadError> {
        self.read_f32_array(2785, count).await
    }

    /// Fetches the current harmonic magnitudes of phase L1, in amperes,
    /// `_FFT_IL1[0..count]` — index 0 is the fundamental (1st harmonic).
    pub async fn current_harmonics_l1(&mut self, count: u16) -> Result<Vec<f32>, ReadError> {
        self.read_f32_array(895, count).await
    }

    /// Fetches the current harmonic phases of phase L1, `_FFT_ILZ1[0..count]`.
    ///
    /// Same unverified-unit caveat as [`Self::voltage_phase_harmonics_l1`].
    pub async fn current_phase_harmonics_l1(&mut self, count: u16) -> Result<Vec<f32>, ReadError> {
        self.read_f32_array(3289, count).await
    }

    /// Fetches the sign of the reactive power of phase L1: `+1` inductive,
    /// `-1` capacitive, `_IND_CAP[0]`.
    pub async fn power_factor_sign_l1(&mut self) -> Result<f32, ReadError> {
        self.read_f32(3987).await
    }

    /// Fetches the fundamental (mains-frequency-only) power factor of phase
    /// L1, `_COS_PHI[0]`.
    pub async fn power_factor_fundamental_l1(&mut self) -> Result<f32, ReadError> {
        self.read_f32(3979).await
    }

    /// Fetches the highest negative sampling value of the current of phase L1
    /// from the last 200ms measuring window, in amperes, `_IL_NEG_PEAK[0]`.
    pub async fn current_peak_negative_l1(&mut self) -> Result<f32, ReadError> {
        self.read_f32(4043).await
    }

    /// Fetches the crest factor (peak / RMS) of the current of phase L1,
    /// `_IL_CF[0]`.
    pub async fn current_crest_factor_l1(&mut self) -> Result<f32, ReadError> {
        self.read_f32(4021).await
    }
}
