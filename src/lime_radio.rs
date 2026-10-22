use std::cell::{RefCell, RefMut};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::Error;
use limesuite::{Device, RxStream, StreamMeta, TxStream};
use log::{debug, info};
use num_complex::Complex32;

use super::{Radio, RadioRx, RadioTx, RxReadResult, TxRadioHealth};

const DEFAULT_FIFO_SIZE: u32 = 1024 * 1024;

const DEFAULT_THROUGHPUT_VS_LATENCY: f32 = 0.0;

fn resolve_antenna_index(device: &Device, dir_tx: bool, chan: usize, name: &str) -> usize {
    if let Ok(list) = device.antenna_list(dir_tx, chan) {
        for (i, entry) in list.iter().enumerate() {
            debug!(
                "Lime: antenna[{}] dir_tx={} chan={}: {}",
                i, dir_tx, chan, entry
            );
            if entry.eq_ignore_ascii_case(name) {
                return i;
            }
        }
    }

    if dir_tx {
        match name.to_ascii_uppercase().as_str() {
            "BAND1" => 0,
            "BAND2" => 1,
            _ => 0,
        }
    } else {
        match name.to_ascii_uppercase().as_str() {
            "LNAH" => 0,
            "LNAL" => 1,
            "LNAW" => 2,
            _ => 2,
        }
    }
}

/// A LimeSDR radio with TX and optional RX. Timestamps count sample-clock ticks.
/// TX and RX can share clocks, so changing one sample rate may affect the other.
/// RX starts immediately on activation.
pub struct LimeRadio {
    rx_rate_hz: f64,
    device: Arc<Device>,
    channel: usize,
    rx_channel: usize,
    rx_bandwidth_hz: f64,
    sample_rate: u64,
    oversample: usize,
    tx_lo_offset_hz: f64,
    tx_sample_rate_hz: f64,
    tx_nco_phase_rad: f64,
    tx_stream: Option<TxStream>,
    rx_stream: Option<RxStream>,
    tx_fifo_size: u32,
    rx_fifo_size: u32,
    throughput_vs_latency: f32,
}

impl LimeRadio {
    /// Opens a LimeSDR selected by the driver string. An empty selector uses the default
    /// device. Channel numbers start at zero. Supply the TX port and gain in dB. An
    /// absent sample rate keeps the device's own rate, for a caller that negotiates one
    /// later. An oversampling factor of zero lets the driver choose.
    /// Stream settings use the defaults described in [`Self::with_stream_config`].
    pub fn new(
        device_str: &str,
        channel: usize,
        tx_antenna: &str,
        tx_gain_db: u32,
        sample_rate_hz: Option<usize>,
        oversample: usize,
    ) -> Result<LimeRadio, Error> {
        Self::with_stream_config(
            device_str,
            channel,
            tx_antenna,
            tx_gain_db,
            sample_rate_hz,
            oversample,
            None,
            None,
            None,
        )
    }

    /// Opens and calibrates TX using the device settings described in [`Self::new`].
    /// FIFO sizes are in samples and default to 1,048,576. Choose a throughput
    /// preference between 0 for lower latency and 1 for higher throughput.
    /// The default is 0. Driver setup or calibration errors are returned.
    pub fn with_stream_config(
        device_str: &str,
        channel: usize,
        tx_antenna: &str,
        tx_gain_db: u32,
        sample_rate_hz: Option<usize>,
        oversample: usize,
        tx_fifo_size: Option<u32>,
        rx_fifo_size: Option<u32>,
        throughput_vs_latency: Option<f32>,
    ) -> Result<LimeRadio, Error> {
        if let Some(rate) = sample_rate_hz {
            crate::stream::positive_hz(rate as f64, "TX sample rate")?;
        }
        u32::try_from(channel).map_err(|_| Error::invalid_setting("Lime channel exceeds u32"))?;
        if tx_fifo_size == Some(0) || rx_fifo_size == Some(0) {
            return Err(Error::invalid_setting(
                "Lime FIFO capacity must be positive",
            ));
        }
        if throughput_vs_latency
            .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
        {
            return Err(Error::invalid_setting(
                "Lime throughput preference must be between zero and one",
            ));
        }
        let info = if device_str.is_empty() {
            None
        } else {
            Some(device_str)
        };
        let device = Arc::new(Device::open(info).map_err(|e| {
            let context = format!("Lime: failed to open device: {}", e);
            Error::from(e).context(context)
        })?);
        info!("Lime: device opened");

        device.init().map_err(|e| {
            let context = format!("Lime: init: {}", e);
            Error::from(e).context(context)
        })?;

        device.enable_channel(true, channel, true).map_err(|e| {
            let context = format!("Lime: enable TX channel: {}", e);
            Error::from(e).context(context)
        })?;

        // Without a requested rate the device keeps the one it came up at, and the
        // readback below reports it. The tick rate follows the sample rate here, so it
        // must never be left at zero.
        if let Some(rate) = sample_rate_hz {
            device
                .set_sample_rate(rate as f64, oversample)
                .map_err(|e| {
                    let context = format!("Lime: set sample rate: {}", e);
                    Error::from(e).context(context)
                })?;
        }
        let actual_rate = device.get_sample_rate(true, channel).map_err(|e| {
            let context = format!("Lime: get sample rate: {}", e);
            Error::from(e).context(context)
        })?;
        info!(
            "Lime: sample_rate requested={:?} actual={} oversample={}",
            sample_rate_hz, actual_rate, oversample
        );

        let tx_ant_idx = resolve_antenna_index(&device, true, channel, tx_antenna);
        device.set_antenna(true, channel, tx_ant_idx).map_err(|e| {
            let context = format!("Lime: set TX antenna: {}", e);
            Error::from(e).context(context)
        })?;
        info!("Lime: TX antenna='{}' (index {})", tx_antenna, tx_ant_idx);

        device.set_gain_db(true, channel, tx_gain_db).map_err(|e| {
            let context = format!("Lime: set TX gain: {}", e);
            Error::from(e).context(context)
        })?;
        info!("Lime: TX gain={}dB", tx_gain_db);

        // Calibrate against the rate the device actually runs, which is the requested
        // rate when there was one and the device default otherwise.
        device.calibrate(true, channel, actual_rate).map_err(|e| {
            let context = format!("Lime: calibrate TX: {}", e);
            Error::from(e).context(context)
        })?;

        let tx_fifo = tx_fifo_size.unwrap_or(DEFAULT_FIFO_SIZE);
        let tvl = throughput_vs_latency.unwrap_or(DEFAULT_THROUGHPUT_VS_LATENCY);
        let tx_stream = TxStream::with_throughput(
            device.clone(),
            u32::try_from(channel)
                .map_err(|_| Error::invalid_setting("Lime channel exceeds u32"))?,
            tx_fifo,
            tvl,
        )
        .map_err(|e| {
            let context = format!("Lime: create TX stream: {}", e);
            Error::from(e).context(context)
        })?;
        info!(
            "Lime: TX stream FIFO size={} samples throughput_vs_latency={:.2}",
            tx_fifo, tvl
        );

        Ok(LimeRadio {
            rx_rate_hz: 0.0,
            device,
            channel,
            rx_channel: channel,
            rx_bandwidth_hz: 0.0,
            sample_rate: actual_rate.round() as u64,
            oversample,
            tx_lo_offset_hz: 0.0,
            tx_sample_rate_hz: actual_rate,
            tx_nco_phase_rad: 0.0,
            tx_stream: Some(tx_stream),
            rx_stream: None,
            tx_fifo_size: tx_fifo,
            rx_fifo_size: rx_fifo_size.unwrap_or(DEFAULT_FIFO_SIZE),
            throughput_vs_latency: tvl,
        })
    }

    /// Sets the TX LO offset in Hz. It takes effect on the next frequency change.
    /// Digital frequency shifting keeps the signal at the requested center frequency.
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

impl Radio for LimeRadio {
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
        (self.rx_stream.is_some()).then_some(self.rx_rate_hz)
    }
    fn supports_timed_tx(&self) -> bool {
        true
    }
    fn tick_rate(&self) -> u64 {
        self.sample_rate
    }

    fn set_tx_frequency(&mut self, center_frequency: usize) -> Result<(), Error> {
        crate::stream::positive_hz(center_frequency as f64, "set_tx_frequency")?;
        let rf_freq = center_frequency as f64 + self.tx_lo_offset_hz;
        self.device
            .set_lo_frequency(true, self.channel, rf_freq)
            .map_err(|e| {
                let context = format!("Lime: set TX freq: {}", e);
                Error::from(e).context(context)
            })?;
        self.device
            .calibrate(true, self.channel, self.sample_rate as f64)
            .map_err(|e| {
                let context = format!("Lime: calibrate TX after freq change: {}", e);
                Error::from(e).context(context)
            })?;
        debug!(
            "Lime: TX freq set to {} (RF={}, LO offset={})",
            center_frequency, rf_freq, self.tx_lo_offset_hz
        );
        Ok(())
    }

    fn set_tx_sample_rate(&mut self, sample_rate_hz: f64) -> Result<f64, Error> {
        crate::stream::positive_hz(sample_rate_hz, "set_tx_sample_rate")?;
        self.device
            .set_sample_rate(sample_rate_hz, self.oversample)
            .map_err(|e| {
                let context = format!("Lime: set sample rate: {}", e);
                Error::from(e).context(context)
            })?;
        let actual = self
            .device
            .get_sample_rate(true, self.channel)
            .map_err(|e| {
                let context = format!("Lime: get sample rate: {}", e);
                Error::from(e).context(context)
            })?;
        self.sample_rate = actual.round() as u64;
        self.tx_sample_rate_hz = actual;
        if self.rx_stream.is_some() {
            self.rx_rate_hz = self
                .device
                .get_sample_rate(false, self.rx_channel)
                .map_err(|e| {
                    let context = format!("Lime: read RX rate: {e}");
                    Error::from(e).context(context)
                })?;
        }
        self.tx_nco_phase_rad = 0.0;
        Ok(self.tx_sample_rate_hz)
    }

    fn set_tx_bandwidth(&mut self, bandwidth: usize) -> Result<(), Error> {
        crate::stream::positive_hz(bandwidth as f64, "set_tx_bandwidth")?;
        self.device
            .set_lpf_bw(true, self.channel, bandwidth as f64)
            .map_err(|e| {
                let context = format!("Lime: set TX LPF BW: {}", e);
                Error::from(e).context(context)
            })?;
        Ok(())
    }

    fn set_tx_lo_offset_hz(&mut self, offset_hz: i64) -> Result<(), Error> {
        self.set_tx_lo_offset(offset_hz)
    }

    fn setup_rx(
        &mut self,
        channel: usize,
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
        u32::try_from(channel).map_err(|_| Error::invalid_setting("Lime channel exceeds u32"))?;
        if let Some(gain) = gain_db {
            crate::stream::whole_u32(gain, "RX gain")?;
        }
        self.rx_channel = channel;
        self.device
            .enable_channel(false, channel, true)
            .map_err(|e| {
                let context = format!("Lime: enable RX channel: {}", e);
                Error::from(e).context(context)
            })?;

        // LimeSDR Mini shares this sample clock between TX and RX.
        if (sample_rate_hz - self.sample_rate as f64).abs() > 1.0 {
            self.device
                .set_sample_rate(sample_rate_hz, self.oversample)
                .map_err(|e| {
                    let context = format!("Lime: set RX sample rate: {}", e);
                    Error::from(e).context(context)
                })?;
            self.sample_rate = sample_rate_hz as u64;
        }

        let rx_ant_idx = resolve_antenna_index(&self.device, false, channel, antenna);
        self.device
            .set_antenna(false, channel, rx_ant_idx)
            .map_err(|e| {
                let context = format!("Lime: set RX antenna: {}", e);
                Error::from(e).context(context)
            })?;

        self.device
            .set_lpf_bw(false, channel, bandwidth_hz)
            .map_err(|e| {
                let context = format!("Lime: set RX LPF BW: {}", e);
                Error::from(e).context(context)
            })?;

        if let Some(gain) = gain_db {
            self.device
                .set_gain_db(false, channel, crate::stream::whole_u32(gain, "RX gain")?)
                .map_err(|e| {
                    let context = format!("Lime: set RX gain: {}", e);
                    Error::from(e).context(context)
                })?;
        }

        self.rx_bandwidth_hz = bandwidth_hz;

        let rx_fifo = self.rx_fifo_size;
        let rx_stream = RxStream::with_throughput(
            self.device.clone(),
            u32::try_from(channel)
                .map_err(|_| Error::invalid_setting("Lime channel exceeds u32"))?,
            rx_fifo,
            self.throughput_vs_latency,
        )
        .map_err(|e| {
            let context = format!("Lime: create RX stream: {}", e);
            Error::from(e).context(context)
        })?;
        self.rx_stream = Some(rx_stream);
        info!(
            "Lime: RX stream FIFO size={} samples throughput_vs_latency={:.2}",
            rx_fifo, self.throughput_vs_latency
        );

        let actual_rate = self.device.get_sample_rate(false, channel).map_err(|e| {
            let context = format!("Lime: read RX rate: {e}");
            Error::from(e).context(context)
        })?;
        info!(
            "Lime: RX configured antenna='{}' (index {}) rate={} bw={} gain={:?}",
            antenna, rx_ant_idx, actual_rate, bandwidth_hz, gain_db
        );
        self.tx_sample_rate_hz = self.device.get_sample_rate(true, self.channel)?;
        crate::stream::positive_hz(self.tx_sample_rate_hz, "applied TX rate")?;
        self.rx_rate_hz = actual_rate;
        self.tx_sample_rate_hz = self
            .device
            .get_sample_rate(true, self.channel)
            .map_err(|e| {
                let context = format!("Lime: read TX rate: {e}");
                Error::from(e).context(context)
            })?;
        self.sample_rate = actual_rate.round() as u64;
        Ok(actual_rate)
    }

    fn split(mut self: Box<Self>) -> Result<(Box<dyn RadioTx>, Option<Box<dyn RadioRx>>), Error> {
        let mut tx_stream = self
            .tx_stream
            .take()
            .ok_or_else(|| Error::from("Lime: TX stream not initialized"))?;
        tx_stream.start().map_err(|e| {
            let context = format!("Lime: start TX stream: {}", e);
            Error::from(e).context(context)
        })?;
        info!("Lime: TX stream started (FPGA timestamp counter active)");

        // Only RX status reports the running FPGA clock. TX status reports the last submitted timestamp.
        let shared_clock = Arc::new(AtomicU64::new(0));

        let device = self.device.clone();
        let tx = LimeTxHalf {
            _device: device.clone(),
            tx_stream,
            sample_rate: self.sample_rate,
            tx_lo_offset_hz: self.tx_lo_offset_hz,
            tx_sample_rate_hz: self.tx_sample_rate_hz,
            tx_nco_phase_rad: self.tx_nco_phase_rad,
            start_of_burst: true,
            shared_clock: shared_clock.clone(),
            last_underrun: 0,
            last_dropped_packets: 0,
            tx_scratch: Vec::with_capacity(self.tx_fifo_size as usize),
            health: TxRadioHealth::default(),
            channel: self.channel,
        };
        let rx_channel = self.rx_channel;
        let rx = self.rx_stream.take().map(|s| -> Box<dyn RadioRx> {
            Box::new(LimeRxHalf {
                rx_rate_hz: self.rx_rate_hz,
                _device: device.clone(),
                rx_stream: RefCell::new(s),
                sample_rate: self.sample_rate,
                shared_clock: shared_clock.clone(),
                rx_read_count: 0,
                last_overrun: 0,
                channel: rx_channel,
                bandwidth_hz: self.rx_bandwidth_hz,
            })
        });
        Ok((Box::new(tx), rx))
    }
}

struct LimeTxHalf {
    _device: Arc<Device>,
    tx_stream: TxStream,
    sample_rate: u64,
    tx_lo_offset_hz: f64,
    tx_sample_rate_hz: f64,
    tx_nco_phase_rad: f64,
    start_of_burst: bool,
    /// Shared clock updated by the RX half from FPGA timestamps.
    shared_clock: Arc<AtomicU64>,
    last_underrun: u32,
    last_dropped_packets: u32,
    tx_scratch: Vec<Complex32>,
    health: TxRadioHealth,
    channel: usize,
}

impl LimeTxHalf {
    fn tx_stream_mut(&mut self) -> &mut TxStream {
        &mut self.tx_stream
    }
}

impl RadioTx for LimeTxHalf {
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
        let phase = self.tx_nco_phase_rad;
        self.tx_scratch.clear();
        self.tx_scratch.extend_from_slice(samples);
        LimeRadio::apply_tx_lo_offset(
            self.tx_lo_offset_hz,
            self.tx_sample_rate_hz,
            &mut self.tx_nco_phase_rad,
            &mut self.tx_scratch,
        );
        let native = StreamMeta {
            timestamp: metadata.schedule.ticks().unwrap_or(0),
            wait_for_timestamp: metadata.schedule.ticks().is_some(),
            flush_partial_packet: metadata.end_of_burst,
        };
        let result = self.tx_stream.send(&self.tx_scratch, &native, 1000);
        match result {
            Ok(count) if count <= samples.len() => {
                self.tx_nco_phase_rad = phase
                    - std::f64::consts::TAU * self.tx_lo_offset_hz * count as f64
                        / self.tx_sample_rate_hz;
                self.start_of_burst = metadata.end_of_burst && count == samples.len();
                Ok(count)
            }
            Ok(count) => Err(Error::incomplete_write(count, samples.len())),
            Err(error) => {
                self.tx_nco_phase_rad = phase;
                Err(error.into())
            }
        }
    }
    fn end_burst(&mut self) -> Result<(), Error> {
        let meta = StreamMeta {
            timestamp: 0,
            wait_for_timestamp: false,
            flush_partial_packet: true,
        };
        self.tx_stream.send(&[], &meta, 100)?;
        self.start_of_burst = true;
        Ok(())
    }

    fn tx_sample_rate_hz(&self) -> f64 {
        self.tx_sample_rate_hz
    }
    fn supports_timed_tx(&self) -> bool {
        true
    }
    fn tick_rate(&self) -> u64 {
        self.sample_rate
    }

    fn get_hardware_time(&self) -> Result<u64, Error> {
        Ok(self.shared_clock.load(Ordering::Relaxed))
    }

    fn set_hardware_time(&self, _ticks: u64) -> Result<(), Error> {
        Err(Error::unsupported("setting hardware time"))
    }

    fn transmit(&mut self, samples: &[Complex32]) -> Result<(), Error> {
        self.transmit_at(samples, None)
    }

    fn prepare_transmit(&mut self, max_samples: usize) -> Result<(), Error> {
        self.tx_scratch.resize(max_samples, Complex32::default());
        self.tx_scratch.clear();
        Ok(())
    }

    fn transmit_at(&mut self, samples: &[Complex32], tick: Option<u64>) -> Result<(), Error> {
        let metadata = crate::TxMetadata {
            schedule: tick.map_or(crate::Schedule::Immediate, |tick| {
                crate::Schedule::At(crate::DeviceTime::from_ticks(tick))
            }),
            end_of_burst: false,
        };
        let count = self.write(samples, metadata)?;
        if count != samples.len() {
            return Err(Error::incomplete_write(count, samples.len()));
        }
        Ok(())
    }

    fn enable_transmit(&mut self, enable: bool) -> Result<(), Error> {
        self.enable_transmit_at(enable, None)
    }

    fn enable_transmit_at(&mut self, enable: bool, tick: Option<u64>) -> Result<(), Error> {
        if tick.is_some() {
            return Err(Error::unsupported("timed TX activation"));
        }
        if enable {
            self.tx_stream.start()?;
            self.tx_nco_phase_rad = 0.0;
            self.start_of_burst = true;
        } else {
            let flush_meta = StreamMeta {
                timestamp: 0,
                wait_for_timestamp: false,
                flush_partial_packet: true,
            };
            let empty: &[Complex32] = &[];
            let flushed = self.tx_stream_mut().send(empty, &flush_meta, 100);
            self.tx_stream_mut().stop().map_err(|e| {
                let context = format!("Lime: TX stream stop: {}", e);
                Error::from(e).context(context)
            })?;
            self.start_of_burst = false;
            flushed?;
        }
        Ok(())
    }

    fn tx_health(&mut self) -> Result<TxRadioHealth, Error> {
        let status = self.tx_stream_mut().status().map_err(|e| {
            let context = format!("Lime: TX stream status: {e}");
            Error::from(e).context(context)
        })?;
        let new_underruns = status.underrun.saturating_sub(self.last_underrun);
        let new_dropped = status
            .dropped_packets
            .saturating_sub(self.last_dropped_packets);
        self.health.underflows += u64::from(new_underruns);
        self.health.dropped_packets += u64::from(new_dropped);
        if new_underruns > 0 || new_dropped > 0 {
            log::warn!(
                "Lime TX stream: underrun={} (+{}) dropped={} (+{}) fifo={}/{}",
                status.underrun,
                new_underruns,
                status.dropped_packets,
                new_dropped,
                status.fifo_filled,
                status.fifo_size,
            );
        }
        self.last_underrun = status.underrun;
        self.last_dropped_packets = status.dropped_packets;
        Ok(self.health)
    }

    fn set_tx_frequency_hz(&mut self, frequency_hz: f64) -> Result<(), Error> {
        crate::stream::positive_hz(frequency_hz, "frequency")?;
        let rf_freq = frequency_hz + self.tx_lo_offset_hz;
        self._device
            .set_lo_frequency(true, self.channel, rf_freq)
            .map_err(|e| {
                let context = format!("Lime: set TX freq: {}", e);
                Error::from(e).context(context)
            })?;
        self._device
            .calibrate(true, self.channel, self.sample_rate as f64)
            .map_err(|e| {
                let context = format!("Lime: calibrate TX after freq change: {}", e);
                Error::from(e).context(context)
            })
    }
}

struct LimeRxHalf {
    rx_rate_hz: f64,
    _device: Arc<Device>,
    rx_stream: RefCell<RxStream>,
    sample_rate: u64,
    /// Shared clock: updated from RX FPGA timestamps so the TX half can read it.
    shared_clock: Arc<AtomicU64>,
    rx_read_count: u64,
    last_overrun: u32,
    channel: usize,
    bandwidth_hz: f64,
}

impl LimeRxHalf {
    fn rx_stream_mut(&self) -> RefMut<'_, RxStream> {
        self.rx_stream.borrow_mut()
    }
}

impl RadioRx for LimeRxHalf {
    fn rx_sample_rate_hz(&self) -> f64 {
        self.rx_rate_hz
    }
    fn tick_rate(&self) -> u64 {
        self.sample_rate
    }

    fn get_hardware_time(&self) -> Result<u64, Error> {
        let st = self.rx_stream_mut().status().map_err(|e| {
            let context = format!("Lime: RX stream status: {}", e);
            Error::from(e).context(context)
        })?;
        self.shared_clock.store(st.timestamp, Ordering::Relaxed);
        Ok(st.timestamp)
    }

    fn rx_read(&mut self, buf: &mut [Complex32], timeout_us: i64) -> Result<RxReadResult, Error> {
        if timeout_us == 0 {
            return Err(Error::unsupported("nonblocking RX"));
        }
        let timeout_ms = if timeout_us < 0 {
            return Err(Error::unsupported("unbounded RX wait"));
        } else {
            u32::try_from((timeout_us as u64).div_ceil(1000))
                .map_err(|_| Error::invalid_setting("RX timeout exceeds u32 milliseconds"))?
        };
        let mut meta = StreamMeta::default();
        let received = self.rx_stream_mut().recv(buf, &mut meta, timeout_ms);
        match received {
            Ok(n) => {
                let end_timestamp = meta.timestamp + n as u64;
                self.shared_clock.store(end_timestamp, Ordering::Relaxed);
                self.rx_read_count += 1;

                if self.rx_read_count.is_multiple_of(400) {
                    let status = self.rx_stream_mut().status();
                    if let Ok(st) = status {
                        let new_overruns = st.overrun.saturating_sub(self.last_overrun);
                        if new_overruns > 0 || st.dropped_packets > 0 {
                            log::warn!(
                                "Lime RX stream: overrun={} (+{}) dropped={} fifo={}/{}",
                                st.overrun,
                                new_overruns,
                                st.dropped_packets,
                                st.fifo_filled,
                                st.fifo_size,
                            );
                        }
                        self.last_overrun = st.overrun;
                    }
                }

                Ok(RxReadResult {
                    samples_read: n,
                    time_ticks: meta.timestamp,
                    overflow: false,
                })
            }
            Err(e) => {
                if let Ok(st) = self.rx_stream_mut().status()
                    && st.overrun > 0
                {
                    return Ok(RxReadResult {
                        samples_read: 0,
                        time_ticks: 0,
                        overflow: true,
                    });
                }
                Err(Error::from(format!("Lime: RX recv: {}", e)))
            }
        }
    }

    fn rx_activate(&mut self, time_ticks: Option<u64>) -> Result<(), Error> {
        if time_ticks.is_some() {
            return Err(Error::unsupported("timed RX activation"));
        }
        self.rx_stream_mut().start().map_err(|e| {
            let context = format!("Lime: RX stream start: {}", e);
            Error::from(e).context(context)
        })?;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), Error> {
        self.rx_stream_mut().stop().map_err(|e| {
            let context = format!("Lime: RX stream stop: {}", e);
            Error::from(e).context(context)
        })?;
        Ok(())
    }

    fn set_rx_frequency(&mut self, frequency_hz: f64) -> Result<(), Error> {
        crate::stream::positive_hz(frequency_hz, "frequency")?;
        self._device
            .set_lo_frequency(false, self.channel, frequency_hz)
            .map_err(|e| {
                let context = format!("Lime: set RX LO: {}", e);
                Error::from(e).context(context)
            })?;
        self._device
            .calibrate(false, self.channel, self.bandwidth_hz)
            .map_err(|e| {
                let context = format!("Lime: calibrate RX: {}", e);
                Error::from(e).context(context)
            })
    }

    fn set_rx_gain(&mut self, gain_db: f64) -> Result<(), Error> {
        crate::stream::finite(gain_db, "gain")?;
        self._device
            .set_gain_db(
                false,
                self.channel,
                crate::stream::whole_u32(gain_db, "RX gain")?,
            )
            .map_err(|e| {
                let context = format!("Lime: set RX gain: {}", e);
                Error::from(e).context(context)
            })
    }
}
