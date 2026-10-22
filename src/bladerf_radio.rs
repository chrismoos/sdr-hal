use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, ErrorKind};
use bladerf::device::{rx_channel, tx_channel};
use bladerf::format::SC16_Q11_META;
use bladerf::meta::{
    FLAG_RX_NOW, FLAG_TX_BURST_END, FLAG_TX_BURST_START, FLAG_TX_NOW, FLAG_TX_UPDATE_TIMESTAMP,
    STATUS_UNDERRUN,
};
use bladerf::stream::{Sc16Q11, StreamMeta};
use bladerf::{Device, RxSync, TxSync};
use log::{debug, info, warn};
use num_complex::Complex32;

use super::{Radio, RadioRx, RadioTx, RxReadResult, TxRadioHealth};

const META_STATUS_RX_FAULT: u32 = bladerf::meta::STATUS_OVERRUN;

const CONTIGUOUS_TIMESTAMP_TOLERANCE_TICKS: u64 = 2;

const DEFAULT_NUM_BUFFERS: u32 = 16;

const DEFAULT_BUFFER_SIZE: u32 = 8192;

const DEFAULT_NUM_TRANSFERS: u32 = 8;

const DEFAULT_STREAM_TIMEOUT_MS: u32 = 3500;

#[derive(Clone, Copy)]
struct SyncStreamConfig {
    num_buffers: u32,
    buffer_size: u32,
    num_transfers: u32,
    timeout_ms: u32,
}

impl SyncStreamConfig {
    fn configure(&self, device: &Device, layout: u32) -> Result<(), Error> {
        device
            .sync_config(
                layout,
                SC16_Q11_META,
                self.num_buffers,
                self.buffer_size,
                self.num_transfers,
                self.timeout_ms,
            )
            .map_err(|error| Error::from(error).context("bladeRF: configure synchronous stream"))
    }
}

/// A bladeRF radio using synchronous TX and optional RX. Timestamps count sample-clock
/// ticks, and samples use the driver's SC16 Q11 format.
/// RX activation starts immediately. The FPGA must already be configured or an image supplied.
pub struct BladeRfRadio {
    rx_rate_hz: f64,
    device: Device,
    channel: u32,
    sample_rate: u32,
    tx_lo_offset_hz: f64,
    tx_sample_rate_hz: f64,
    tx_nco_phase_rad: f64,
    rx_configured: bool,
    num_buffers: u32,
    buffer_size: u32,
    num_transfers: u32,
    stream_timeout_ms: u32,
}

impl BladeRfRadio {
    /// Opens a libbladeRF selector and zero-based channel at the supplied TX rate in Hz
    /// and gain in dB. An absent sample rate keeps the device's own rate, for a caller
    /// that negotiates one later. Empty selector chooses the default device. `fpga_path` is used
    /// only when no FPGA image is loaded. Antenna is a native RF port name.
    /// Stream defaults are 16 buffers, 8192 samples per buffer, 8 simultaneous
    /// transfers, and 3500 ms timeout. Returns FPGA, configuration, or driver errors.
    pub fn with_stream_config(
        device_id: &str,
        channel: u32,
        tx_gain_db: i32,
        sample_rate_hz: Option<u32>,
        fpga_path: Option<&str>,
        tx_antenna: Option<&str>,
        num_buffers: Option<u32>,
        buffer_size: Option<u32>,
        num_transfers: Option<u32>,
        stream_timeout_ms: Option<u32>,
    ) -> Result<Self, Error> {
        if let Some(rate) = sample_rate_hz {
            crate::stream::positive_hz(rate as f64, "TX sample rate")?;
        }
        let id = if device_id.is_empty() {
            None
        } else {
            Some(device_id)
        };
        let device = Device::open(id).map_err(|e| {
            let context = format!("bladeRF: open: {}", e);
            Error::from(e).context(context)
        })?;

        let board = device.board_name();
        info!("bladeRF: opened board={}", board);

        if let Ok(serial) = device.serial() {
            info!("bladeRF: serial={}", serial);
        }

        let fpga_loaded = device.is_fpga_configured().map_err(|e| {
            let context = format!("bladeRF: check FPGA: {}", e);
            Error::from(e).context(context)
        })?;
        if !fpga_loaded {
            match fpga_path {
                Some(path) => {
                    info!("bladeRF: FPGA not loaded, loading from {}", path);
                    device.load_fpga(path).map_err(|e| {
                        let context = format!("bladeRF: load FPGA: {}", e);
                        Error::from(e).context(context)
                    })?;
                    info!("bladeRF: FPGA loaded successfully");
                }
                None => {
                    return Err(Error::from(
                        "bladeRF: FPGA not configured and no fpga_path specified. \
                         Place the correct .rbf bitstream (e.g. hostedxA4.rbf for Micro 2.0) \
                         in ~/.config/Nuand/bladeRF/ for automatic loading, flash it to SPI \
                         with `bladeRF-cli -L <file>.rbf`, or set fpga_path in the radio config.",
                    ));
                }
            }
        } else {
            info!("bladeRF: FPGA already configured");
        }

        let tx_ch = tx_channel(channel);

        if let Ok(ports) = device.get_rf_ports(tx_ch) {
            info!("bladeRF: available TX RF ports: {:?}", ports);
        }

        if let Some(antenna) = tx_antenna {
            device.set_rf_port(tx_ch, antenna).map_err(|e| {
                let context = format!("bladeRF: set TX RF port: {}", e);
                Error::from(e).context(context)
            })?;
            info!("bladeRF: TX RF port set to '{}'", antenna);
        }
        if let Ok(port) = device.get_rf_port(tx_ch) {
            info!("bladeRF: TX RF port active: '{}'", port);
        }

        // Without a requested rate the device keeps the one it came up at. The tick rate
        // follows the sample rate here, so it must never be left at zero.
        let actual_rate = match sample_rate_hz {
            Some(rate) => device.set_sample_rate(tx_ch, rate).map_err(|e| {
                let context = format!("bladeRF: set TX sample rate: {}", e);
                Error::from(e).context(context)
            })?,
            None => device.get_sample_rate(tx_ch).map_err(|e| {
                let context = format!("bladeRF: get TX sample rate: {}", e);
                Error::from(e).context(context)
            })?,
        };
        info!(
            "bladeRF: TX sample_rate requested={:?} actual={}",
            sample_rate_hz, actual_rate
        );

        device.set_gain(tx_ch, tx_gain_db).map_err(|e| {
            let context = format!("bladeRF: set TX gain: {}", e);
            Error::from(e).context(context)
        })?;
        info!("bladeRF: TX gain={}dB", tx_gain_db);

        let nb = num_buffers.unwrap_or(DEFAULT_NUM_BUFFERS);
        let bs = buffer_size.unwrap_or(DEFAULT_BUFFER_SIZE);
        let nt = num_transfers.unwrap_or(DEFAULT_NUM_TRANSFERS);
        let st = stream_timeout_ms.unwrap_or(DEFAULT_STREAM_TIMEOUT_MS);

        Ok(BladeRfRadio {
            rx_rate_hz: 0.0,
            device,
            channel,
            sample_rate: actual_rate,
            tx_lo_offset_hz: 0.0,
            tx_sample_rate_hz: actual_rate as f64,
            tx_nco_phase_rad: 0.0,
            rx_configured: false,
            num_buffers: nb,
            buffer_size: bs,
            num_transfers: nt,
            stream_timeout_ms: st,
        })
    }

    /// Sets the TX LO offset in Hz. Digital frequency shifting compensates for the offset.
    /// Retune the center frequency after changing the offset to apply it to the LO.
    pub fn set_tx_lo_offset(&mut self, offset_hz: i64) -> Result<(), Error> {
        self.tx_lo_offset_hz = offset_hz as f64;
        self.tx_nco_phase_rad = 0.0;
        Ok(())
    }

    fn apply_tx_lo_offset(
        tx_lo_offset_hz: f64,
        tx_sample_rate_hz: f64,
        tx_nco_phase_rad: &mut f64,
        samples: &mut [Complex32],
    ) {
        if tx_lo_offset_hz == 0.0 || tx_sample_rate_hz <= 0.0 {
            return;
        }

        let phase_step = -2.0 * std::f64::consts::PI * tx_lo_offset_hz / tx_sample_rate_hz;
        let mut phase = *tx_nco_phase_rad;

        for sample in samples.iter_mut() {
            let rot = Complex32::new(phase.cos() as f32, phase.sin() as f32);
            *sample *= rot;
            phase += phase_step;
        }

        *tx_nco_phase_rad = phase.rem_euclid(2.0 * std::f64::consts::PI);
    }
}

impl Radio for BladeRfRadio {
    fn tx_capabilities(&self) -> crate::TxCapabilities {
        let capabilities = crate::TxCapabilities::from([crate::TxCapability::BurstEnd]);
        if self.supports_timed_tx() {
            capabilities.with(crate::TxCapability::TimedSamples)
        } else {
            capabilities
        }
    }

    fn tx_sample_rate_hz(&self) -> f64 {
        self.tx_sample_rate_hz
    }
    fn rx_sample_rate_hz(&self) -> Option<f64> {
        (self.rx_configured).then_some(self.rx_rate_hz)
    }
    fn supports_timed_tx(&self) -> bool {
        true
    }
    fn tick_rate(&self) -> u64 {
        self.sample_rate as u64
    }

    fn set_tx_frequency(&mut self, center_frequency: usize) -> Result<(), Error> {
        crate::stream::positive_hz(center_frequency as f64, "set_tx_frequency")?;
        let rf_freq = crate::stream::frequency_u64(center_frequency as f64 + self.tx_lo_offset_hz)?;
        self.device
            .set_frequency(tx_channel(self.channel), rf_freq)
            .map_err(|e| {
                let context = format!("bladeRF: set TX freq: {}", e);
                Error::from(e).context(context)
            })?;
        debug!(
            "bladeRF: TX freq set to {} (RF={}, LO offset={})",
            center_frequency, rf_freq, self.tx_lo_offset_hz
        );
        Ok(())
    }

    fn set_tx_sample_rate(&mut self, sample_rate_hz: f64) -> Result<f64, Error> {
        crate::stream::positive_hz(sample_rate_hz, "set_tx_sample_rate")?;
        // TODO: use libbladeRF's rational sample rate API so fractional rates apply exactly.
        let rounded_hz = sample_rate_hz.round();
        if rounded_hz > u32::MAX as f64 {
            return Err(Error::invalid_setting(format!(
                "bladeRF: TX sample rate {sample_rate_hz} Hz exceeds u32"
            )));
        }
        let actual = self
            .device
            .set_sample_rate(tx_channel(self.channel), rounded_hz as u32)
            .map_err(|e| {
                let context = format!("bladeRF: set TX sample rate: {}", e);
                Error::from(e).context(context)
            })?;
        self.sample_rate = actual;
        self.tx_sample_rate_hz = actual as f64;
        self.tx_nco_phase_rad = 0.0;
        Ok(actual as f64)
    }

    fn set_tx_bandwidth(&mut self, bandwidth: usize) -> Result<(), Error> {
        crate::stream::positive_hz(bandwidth as f64, "set_tx_bandwidth")?;
        self.device
            .set_bandwidth(
                tx_channel(self.channel),
                u32::try_from(bandwidth)
                    .map_err(|_| Error::invalid_setting("TX bandwidth exceeds u32"))?,
            )
            .map_err(|e| {
                let context = format!("bladeRF: set TX bandwidth: {}", e);
                Error::from(e).context(context)
            })?;
        Ok(())
    }

    fn set_tx_lo_offset_hz(&mut self, offset_hz: i64) -> Result<(), Error> {
        self.set_tx_lo_offset(offset_hz)
    }

    fn setup_rx(
        &mut self,
        _channel: usize,
        antenna: &str,
        sample_rate_hz: f64,
        bandwidth_hz: f64,
        gain_db: Option<f64>,
    ) -> Result<f64, Error> {
        crate::stream::positive_hz(sample_rate_hz, "RX sample rate")?;
        crate::stream::positive_hz(bandwidth_hz, "RX bandwidth")?;
        if let Some(gain) = gain_db {
            crate::stream::finite(gain, "RX gain")?;
        }
        checked_u32_hz(sample_rate_hz, "RX sample rate")?;
        checked_u32_hz(bandwidth_hz, "RX bandwidth")?;
        if let Some(gain) = gain_db {
            crate::stream::whole_i32(gain, "RX gain")?;
        }
        let rx_ch = rx_channel(self.channel);

        if let Ok(ports) = self.device.get_rf_ports(rx_ch) {
            info!("bladeRF: available RX RF ports: {:?}", ports);
        }
        if !antenna.is_empty() {
            self.device.set_rf_port(rx_ch, antenna).map_err(|e| {
                let context = format!("bladeRF: set RX RF port: {}", e);
                Error::from(e).context(context)
            })?;
            info!("bladeRF: RX RF port set to '{}'", antenna);
        }
        if let Ok(port) = self.device.get_rf_port(rx_ch) {
            info!("bladeRF: RX RF port active: '{}'", port);
        }

        let actual_rate = self
            .device
            .set_sample_rate(rx_ch, checked_u32_hz(sample_rate_hz, "RX sample rate")?)
            .map_err(|e| {
                let context = format!("bladeRF: set RX sample rate: {}", e);
                Error::from(e).context(context)
            })?;

        let actual_bandwidth = self
            .device
            .set_bandwidth(rx_ch, checked_u32_hz(bandwidth_hz, "RX bandwidth")?)
            .map_err(|e| {
                let context = format!("bladeRF: set RX bandwidth: {}", e);
                Error::from(e).context(context)
            })?;

        if let Some(gain) = gain_db {
            self.device
                .set_gain_mode(rx_ch, bladerf::gain_mode::MGC)
                .map_err(|e| {
                    let context = format!("bladeRF: set RX gain mode: {}", e);
                    Error::from(e).context(context)
                })?;
            self.device
                .set_gain(rx_ch, crate::stream::whole_i32(gain, "RX gain")?)
                .map_err(|e| {
                    let context = format!("bladeRF: set RX gain: {}", e);
                    Error::from(e).context(context)
                })?;
        }

        let rx_layout = bladerf::layout::RX_X1;
        self.device
            .sync_config(
                rx_layout,
                SC16_Q11_META,
                self.num_buffers,
                self.buffer_size,
                self.num_transfers,
                self.stream_timeout_ms,
            )
            .map_err(|e| {
                let context = format!("bladeRF: sync_config RX: {}", e);
                Error::from(e).context(context)
            })?;

        self.rx_rate_hz = actual_rate as f64;
        self.rx_configured = true;

        info!(
            "bladeRF: RX configured rate={} bw={} gain={:?}",
            actual_rate, actual_bandwidth, gain_db
        );
        Ok(actual_rate as f64)
    }

    fn split(self: Box<Self>) -> Result<(Box<dyn RadioTx>, Option<Box<dyn RadioRx>>), Error> {
        let device = Arc::new(self.device);
        let shared_clock = Arc::new(AtomicU64::new(0));
        let stream_config = SyncStreamConfig {
            num_buffers: self.num_buffers,
            buffer_size: self.buffer_size,
            num_transfers: self.num_transfers,
            timeout_ms: self.stream_timeout_ms,
        };

        let tx_layout = bladerf::layout::TX_X1;
        stream_config.configure(&device, tx_layout)?;
        info!(
            "bladeRF: TX sync configured buffers={} buf_size={} transfers={} timeout={}ms",
            self.num_buffers, self.buffer_size, self.num_transfers, self.stream_timeout_ms
        );

        device
            .enable_module(tx_channel(self.channel), true)
            .map_err(|e| {
                let context = format!("bladeRF: enable TX module: {}", e);
                Error::from(e).context(context)
            })?;
        info!("bladeRF: TX module enabled");

        let tx_sync = TxSync::new(&device);
        let tx = BladeRfTxHalf {
            _device: device.clone(),
            tx_sync,
            stream_config,
            channel: self.channel,
            sample_rate: self.sample_rate,
            tx_lo_offset_hz: self.tx_lo_offset_hz,
            tx_sample_rate_hz: self.tx_sample_rate_hz,
            tx_nco_phase_rad: self.tx_nco_phase_rad,
            shared_clock: shared_clock.clone(),
            stream_timeout_ms: self.stream_timeout_ms,
            module_enabled: true,
            burst_active: false,
            next_timestamp: None,
            tx_scratch: Vec::with_capacity(self.buffer_size as usize),
            sc16_scratch: Vec::with_capacity(self.buffer_size as usize),
            health: TxRadioHealth::default(),
        };

        let rx = if self.rx_configured {
            device
                .enable_module(rx_channel(self.channel), true)
                .map_err(|e| {
                    let context = format!("bladeRF: enable RX module: {}", e);
                    Error::from(e).context(context)
                })?;
            info!("bladeRF: RX module enabled");

            let rx_sync = RxSync::new(&device);
            Some(Box::new(BladeRfRxHalf {
                rx_rate_hz: self.rx_rate_hz,
                _device: device.clone(),
                rx_sync,
                stream_config,
                channel: self.channel,
                sample_rate: self.rx_rate_hz.round() as u32,
                shared_clock: shared_clock.clone(),
                module_enabled: true,
                sc16_scratch: Vec::with_capacity(self.buffer_size as usize),
            }) as Box<dyn RadioRx>)
        } else {
            None
        };

        Ok((Box::new(tx), rx))
    }
}

struct BladeRfTxHalf {
    _device: Arc<Device>,
    tx_sync: TxSync,
    stream_config: SyncStreamConfig,
    channel: u32,
    sample_rate: u32,
    tx_lo_offset_hz: f64,
    tx_sample_rate_hz: f64,
    tx_nco_phase_rad: f64,
    shared_clock: Arc<AtomicU64>,
    stream_timeout_ms: u32,
    module_enabled: bool,
    burst_active: bool,
    next_timestamp: Option<u64>,
    tx_scratch: Vec<Complex32>,
    sc16_scratch: Vec<Sc16Q11>,
    health: TxRadioHealth,
}

impl RadioTx for BladeRfTxHalf {
    fn capabilities(&self) -> crate::TxCapabilities {
        crate::TxCapabilities::from([
            crate::TxCapability::TimedSamples,
            crate::TxCapability::BurstEnd,
        ])
    }
    fn write(
        &mut self,
        samples: &[Complex32],
        metadata: crate::TxMetadata,
    ) -> Result<usize, Error> {
        self.write_chunk(samples, metadata.schedule.ticks(), metadata.end_of_burst)?;
        Ok(samples.len())
    }
    fn end_burst(&mut self) -> Result<(), Error> {
        self.enable_transmit(false)
    }
    fn stop(&mut self) -> Result<(), Error> {
        let ended = self.end_burst();
        self._device
            .enable_module(tx_channel(self.channel), false)?;
        self.module_enabled = false;
        ended
    }

    fn tx_sample_rate_hz(&self) -> f64 {
        self.tx_sample_rate_hz
    }
    fn supports_timed_tx(&self) -> bool {
        true
    }
    fn tick_rate(&self) -> u64 {
        self.sample_rate as u64
    }

    fn get_hardware_time(&self) -> Result<u64, Error> {
        match self._device.get_timestamp(bladerf::direction::TX) {
            Ok(ts) if ts > 0 => Ok(ts),
            _ => Ok(self.shared_clock.load(Ordering::Relaxed)),
        }
    }

    fn set_hardware_time(&self, _ticks: u64) -> Result<(), Error> {
        Err(Error::unsupported("setting hardware time"))
    }

    fn transmit(&mut self, samples: &[Complex32]) -> Result<(), Error> {
        self.transmit_at(samples, None)
    }

    fn prepare_transmit(&mut self, max_samples: usize) -> Result<(), Error> {
        self.tx_scratch.resize(max_samples, Complex32::default());
        self.sc16_scratch
            .resize(max_samples, Sc16Q11 { i: 0, q: 0 });
        self.tx_scratch.clear();
        self.sc16_scratch.clear();
        Ok(())
    }

    fn transmit_at(&mut self, samples: &[Complex32], tick: Option<u64>) -> Result<(), Error> {
        self.write_chunk(samples, tick, false)
    }

    fn enable_transmit(&mut self, enable: bool) -> Result<(), Error> {
        if enable {
            if !self.module_enabled {
                // Disabling a bladeRF module tears down its synchronous stream.
                self.stream_config
                    .configure(&self._device, bladerf::layout::TX_X1)?;
                self._device.enable_module(tx_channel(self.channel), true)?;
                self.module_enabled = true;
            }
            if !self.burst_active {
                self.tx_nco_phase_rad = 0.0;
            }
        } else if self.burst_active {
            // Ending a burst must not disable the TX channel, which destroys the stream.
            let zero = [Sc16Q11 { i: 0, q: 0 }; 1];
            let mut meta = StreamMeta {
                flags: FLAG_TX_BURST_END,
                ..Default::default()
            };
            let result = self
                .tx_sync
                .send(&zero, Some(&mut meta), self.stream_timeout_ms)
                .map_err(|e| {
                    let context = format!("bladeRF: end TX burst: {}", e);
                    Error::from(e).context(context)
                });
            self.burst_active = false;
            self.next_timestamp = None;
            result?;
        }
        Ok(())
    }

    fn enable_transmit_at(&mut self, enable: bool, tick: Option<u64>) -> Result<(), Error> {
        if tick.is_some() {
            return Err(Error::unsupported("timed TX activation"));
        }
        self.enable_transmit(enable)
    }

    fn tx_health(&mut self) -> Result<TxRadioHealth, Error> {
        Ok(self.health)
    }

    fn set_tx_frequency_hz(&mut self, frequency_hz: f64) -> Result<(), Error> {
        crate::stream::positive_hz(frequency_hz, "frequency")?;
        let rf_freq = crate::stream::frequency_u64(frequency_hz + self.tx_lo_offset_hz)?;
        self._device
            .set_frequency(tx_channel(self.channel), rf_freq)
            .map_err(|e| {
                let context = format!("bladeRF: set TX freq: {}", e);
                Error::from(e).context(context)
            })
    }
}

impl Drop for BladeRfTxHalf {
    fn drop(&mut self) {
        if self.burst_active {
            let zero = [Sc16Q11 { i: 0, q: 0 }; 1];
            let mut meta = StreamMeta {
                flags: FLAG_TX_BURST_END,
                ..Default::default()
            };
            let _ = self
                .tx_sync
                .send(&zero, Some(&mut meta), self.stream_timeout_ms);
        }
        let _ = self._device.enable_module(tx_channel(self.channel), false);
    }
}

struct BladeRfRxHalf {
    rx_rate_hz: f64,
    _device: Arc<Device>,
    rx_sync: RxSync,
    stream_config: SyncStreamConfig,
    channel: u32,
    sample_rate: u32,
    shared_clock: Arc<AtomicU64>,
    module_enabled: bool,
    sc16_scratch: Vec<Sc16Q11>,
}

#[derive(Debug, PartialEq, Eq)]
struct BladeRfRxStatus {
    overflow: bool,
    hardware_status: bool,
}

fn classify_rx_status(
    metadata_status: u32,
    samples_read: usize,
    requested_samples: usize,
) -> BladeRfRxStatus {
    let fault = metadata_status & META_STATUS_RX_FAULT != 0;
    let overflow = fault && samples_read < requested_samples;
    BladeRfRxStatus {
        overflow,
        hardware_status: fault && !overflow,
    }
}

impl RadioRx for BladeRfRxHalf {
    fn rx_sample_rate_hz(&self) -> f64 {
        self.rx_rate_hz
    }
    fn tick_rate(&self) -> u64 {
        self.sample_rate as u64
    }

    fn get_hardware_time(&self) -> Result<u64, Error> {
        match self._device.get_timestamp(bladerf::direction::RX) {
            Ok(ts) => {
                self.shared_clock.store(ts, Ordering::Relaxed);
                Ok(ts)
            }
            Err(_) => Ok(self.shared_clock.load(Ordering::Relaxed)),
        }
    }

    fn rx_read(&mut self, buf: &mut [Complex32], timeout_us: i64) -> Result<RxReadResult, Error> {
        if timeout_us == 0 {
            return Err(Error::unsupported("nonblocking RX"));
        }
        let timeout_ms = if timeout_us < 0 {
            0
        } else {
            u32::try_from((timeout_us as u64).div_ceil(1000))
                .map_err(|_| Error::invalid_setting("RX timeout exceeds u32 milliseconds"))?
        };

        self.sc16_scratch.resize(buf.len(), Sc16Q11::default());
        let mut meta = StreamMeta {
            flags: FLAG_RX_NOW,
            ..Default::default()
        };

        match self
            .rx_sync
            .recv(&mut self.sc16_scratch, &mut meta, timeout_ms)
        {
            Ok(n) => {
                for i in 0..n.min(buf.len()) {
                    buf[i] = self.sc16_scratch[i].to_complex32();
                }

                let end_ts = meta.timestamp + n as u64;
                self.shared_clock.store(end_ts, Ordering::Relaxed);
                let status = classify_rx_status(meta.status, n, buf.len());
                if status.hardware_status {
                    log::trace!(
                        "bladeRF: metadata status bit 0 set on full RX read; \
                         samples={} timestamp={} raw_status={:#010x}",
                        n,
                        meta.timestamp,
                        meta.status,
                    );
                }

                Ok(RxReadResult {
                    samples_read: n,
                    time_ticks: meta.timestamp,
                    overflow: status.overflow,
                })
            }
            Err(e) => Err(Error::from(format!("bladeRF: RX recv: {}", e))),
        }
    }

    fn rx_activate(&mut self, time_ticks: Option<u64>) -> Result<(), Error> {
        if time_ticks.is_some() {
            return Err(Error::unsupported("timed RX activation"));
        }
        if !self.module_enabled {
            self.stream_config
                .configure(&self._device, bladerf::layout::RX_X1)?;
            self._device
                .enable_module(rx_channel(self.channel), true)
                .map_err(|e| {
                    let context = format!("bladeRF: enable RX: {}", e);
                    Error::from(e).context(context)
                })?;
            self.module_enabled = true;
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<(), Error> {
        if self.module_enabled {
            self._device
                .enable_module(rx_channel(self.channel), false)
                .map_err(|e| {
                    let context = format!("bladeRF: disable RX: {}", e);
                    Error::from(e).context(context)
                })?;
            self.module_enabled = false;
        }
        Ok(())
    }

    fn set_rx_frequency(&mut self, frequency_hz: f64) -> Result<(), Error> {
        crate::stream::positive_hz(frequency_hz, "frequency")?;
        self._device
            .set_frequency(
                rx_channel(self.channel),
                crate::stream::frequency_u64(frequency_hz)?,
            )
            .map_err(|e| {
                let context = format!("bladeRF: set RX freq: {}", e);
                Error::from(e).context(context)
            })
    }

    fn set_rx_gain(&mut self, gain_db: f64) -> Result<(), Error> {
        crate::stream::finite(gain_db, "gain")?;
        let rx_ch = rx_channel(self.channel);
        self._device
            .set_gain_mode(rx_ch, bladerf::gain_mode::MGC)
            .map_err(|e| {
                let context = format!("bladeRF: set RX gain mode: {}", e);
                Error::from(e).context(context)
            })?;
        self._device
            .set_gain(rx_ch, crate::stream::whole_i32(gain_db, "RX gain")?)
            .map_err(|e| {
                let context = format!("bladeRF: set RX gain: {}", e);
                Error::from(e).context(context)
            })
    }
}

impl Drop for BladeRfRxHalf {
    fn drop(&mut self) {
        let _ = self._device.enable_module(rx_channel(self.channel), false);
    }
}

impl BladeRfTxHalf {
    fn write_chunk(
        &mut self,
        samples: &[Complex32],
        tick: Option<u64>,
        end_of_burst: bool,
    ) -> Result<(), Error> {
        self.tx_scratch.clear();
        self.tx_scratch.extend_from_slice(samples);
        BladeRfRadio::apply_tx_lo_offset(
            self.tx_lo_offset_hz,
            self.tx_sample_rate_hz,
            &mut self.tx_nco_phase_rad,
            &mut self.tx_scratch,
        );
        self.sc16_scratch.clear();
        self.sc16_scratch
            .extend(self.tx_scratch.iter().map(|s| Sc16Q11::from_complex32(*s)));

        match tick {
            Some(ts) => {
                let starting_burst = !self.burst_active;
                let mut meta = if starting_burst {
                    StreamMeta {
                        timestamp: ts,
                        flags: FLAG_TX_BURST_START,
                        ..Default::default()
                    }
                } else if self
                    .next_timestamp
                    .is_some_and(|next| next.abs_diff(ts) <= CONTIGUOUS_TIMESTAMP_TOLERANCE_TICKS)
                {
                    StreamMeta::default()
                } else {
                    StreamMeta {
                        timestamp: ts,
                        flags: FLAG_TX_UPDATE_TIMESTAMP,
                        ..Default::default()
                    }
                };
                if end_of_burst {
                    meta.flags |= FLAG_TX_BURST_END;
                }
                let send_started = std::time::Instant::now();
                if starting_burst {
                    let tx_now = self._device.get_timestamp(bladerf::direction::TX)?;
                    let rx_now = self.shared_clock.load(Ordering::Relaxed);
                    let lead_ms =
                        (ts as i128 - tx_now as i128) as f64 * 1000.0 / self.tx_sample_rate_hz;
                    info!(
                        "bladeRF: timed TX burst target={} tx_now={} rx_anchor={} lead_ms={:.3} samples={} end_of_burst={}",
                        ts,
                        tx_now,
                        rx_now,
                        lead_ms,
                        samples.len(),
                        end_of_burst
                    );
                    if ts <= tx_now {
                        self.health.late_packets += 1;
                        return Err(Error::new(
                            ErrorKind::Late,
                            format!(
                                "bladeRF: TX deadline passed before submission target={ts} tx_now={tx_now}"
                            ),
                        ));
                    }
                }
                match self
                    .tx_sync
                    .send(&self.sc16_scratch, Some(&mut meta), self.stream_timeout_ms)
                {
                    Ok(()) => {
                        self.burst_active = !end_of_burst;
                        self.next_timestamp = Some(ts.saturating_add(samples.len() as u64));
                    }
                    Err(e) if e.is_late() => {
                        self.health.late_packets += 1;
                        warn!("bladeRF: TX late @{}: {}", ts, e);
                        let context = format!("bladeRF: TX late @{ts}: {e}");
                        return Err(Error::from(e).context(context));
                    }
                    Err(e) => {
                        let tx_now = self._device.get_timestamp(bladerf::direction::TX);
                        let context = format!(
                            "bladeRF: TX send @{ts}: {e} (elapsed_ms={} samples={} tx_now={tx_now:?} rx_anchor={} end_of_burst={end_of_burst})",
                            send_started.elapsed().as_millis(),
                            samples.len(),
                            self.shared_clock.load(Ordering::Relaxed),
                        );
                        return Err(Error::from(e).context(context));
                    }
                }
                if meta.status & STATUS_UNDERRUN != 0 {
                    self.health.underflows += 1;
                    warn!("bladeRF: TX underrun detected");
                }
                Ok(())
            }
            None => {
                let mut flags = FLAG_TX_NOW;
                if !self.burst_active {
                    flags |= FLAG_TX_BURST_START;
                }
                let mut meta = StreamMeta {
                    flags,
                    ..Default::default()
                };
                if end_of_burst {
                    meta.flags |= FLAG_TX_BURST_END;
                }
                self.tx_sync
                    .send(&self.sc16_scratch, Some(&mut meta), self.stream_timeout_ms)
                    .map_err(|e| {
                        let context = format!("bladeRF: TX send: {}", e);
                        Error::from(e).context(context)
                    })?;
                self.burst_active = !end_of_burst;
                self.next_timestamp = None;
                if meta.status & STATUS_UNDERRUN != 0 {
                    self.health.underflows += 1;
                    warn!("bladeRF: TX underrun detected");
                }
                Ok(())
            }
        }
    }
}

fn checked_u32_hz(value: f64, name: &str) -> Result<u32, Error> {
    crate::stream::positive_hz(value, name)?;
    if value > u32::MAX as f64 || value.fract() != 0.0 {
        return Err(Error::invalid_setting(format!(
            "{name} must fit a whole-Hz u32, got {value}"
        )));
    }
    Ok(value as u32)
}

#[cfg(test)]
mod tests {
    use super::{META_STATUS_RX_FAULT, classify_rx_status};

    #[test]
    fn full_rx_read_with_fault_bit_is_hardware_status() {
        let status = classify_rx_status(META_STATUS_RX_FAULT, 12_288, 12_288);

        assert!(!status.overflow);
        assert!(status.hardware_status);
    }

    #[test]
    fn short_rx_read_with_fault_bit_is_overflow() {
        let status = classify_rx_status(META_STATUS_RX_FAULT, 8_188, 12_288);

        assert!(status.overflow);
        assert!(!status.hardware_status);
    }

    #[test]
    fn full_rx_read_without_fault_bit_is_clean() {
        let status = classify_rx_status(0, 12_288, 12_288);

        assert!(!status.overflow);
        assert!(!status.hardware_status);
    }
}
