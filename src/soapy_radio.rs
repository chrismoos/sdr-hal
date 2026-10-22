use crate::error::Error;
use log::{debug, info, warn};
use num_complex::Complex32;
use soapysdr::{Direction, RxStream, TxStream};

use super::{Radio, RadioRx, RadioTx, RxReadResult, TxRadioHealth};

const SOAPY_END_BURST: u32 = 2;

const MAX_TX_STATUS_EVENTS_PER_POLL: usize = 256;
const TX_WRITE_TIMEOUT_US: i64 = 1_000_000;

/// A SoapySDR radio with TX and optional RX. Timestamps are in nanoseconds.
/// Timed TX requires a driver that supports hardware time.
pub struct SoapySdrRadio {
    rx_rate_hz: f64,
    device: soapysdr::Device,
    channel: usize,
    rx_channel: usize,
    stream: TxStream<num_complex::Complex<f32>>,
    tx_lo_offset_hz: f64,
    tx_sample_rate_hz: f64,
    tx_nco_phase_rad: f64,
    rx_stream: Option<RxStream<Complex32>>,
}

impl SoapySdrRadio {
    /// Opens the device selected by the SoapySDR arguments. Channel numbers start at
    /// zero. Supply a TX port name and gain in dB. An absent sample rate keeps the
    /// driver's own rate, for a caller that negotiates one later.
    /// Returns an error if device or stream setup fails.
    pub fn new(
        device_str: &str,
        channel: usize,
        antenna: &str,
        tx_gain_db: f64,
        tx_sample_rate_hz: Option<usize>,
    ) -> Result<SoapySdrRadio, Error> {
        if let Some(rate) = tx_sample_rate_hz {
            crate::stream::positive_hz(rate as f64, "SoapySDR TX sample rate")?;
        }
        crate::stream::finite(tx_gain_db, "TX gain")?;
        let device = soapysdr::Device::new(device_str)?;
        if let Some(rate) = tx_sample_rate_hz {
            device.set_sample_rate(Direction::Tx, channel, rate as f64)?;
        }
        let actual_rate_hz = device.sample_rate(Direction::Tx, channel)?;
        for antenna in device.antennas(soapysdr::Direction::Tx, channel)? {
            debug!("Antenna: {}", antenna);
        }
        for format in device.stream_formats(soapysdr::Direction::Tx, channel)? {
            debug!("SDR format: {:?}", format);
        }

        device.set_gain(soapysdr::Direction::Tx, channel, tx_gain_db)?;
        device.set_antenna(soapysdr::Direction::Tx, channel, antenna)?;

        let stream = device.tx_stream::<_>(&[channel])?;

        Ok(SoapySdrRadio {
            rx_rate_hz: 0.0,
            device,
            channel,
            rx_channel: channel,
            stream,
            tx_lo_offset_hz: 0.0,
            tx_sample_rate_hz: actual_rate_hz,
            tx_nco_phase_rad: 0.0,
            rx_stream: None,
        })
    }
}

impl SoapySdrRadio {
    /// Creates an inactive SoapySDR RX stream and returns it directly. Channel numbers
    /// start at zero. Rate and bandwidth are in Hz, and gain is in dB.
    /// The returned stream will not be part of [`Radio::split`]. Use [`Radio::setup_rx`]
    /// if you want the radio to keep it until splitting.
    pub fn setup_rx_stream(
        &self,
        channel: usize,
        antenna: &str,
        sample_rate_hz: f64,
        bandwidth_hz: f64,
        gain_db: Option<f64>,
    ) -> Result<RxStream<Complex32>, Error> {
        debug!(
            "setup_rx: antenna={} rate={} bw={}",
            antenna, sample_rate_hz, bandwidth_hz
        );
        self.device.set_antenna(Direction::Rx, channel, antenna)?;
        self.device
            .set_sample_rate(Direction::Rx, channel, sample_rate_hz)?;
        self.device
            .set_bandwidth(Direction::Rx, channel, bandwidth_hz)?;
        if let Some(gain) = gain_db {
            self.device.set_gain(Direction::Rx, channel, gain)?;
        }
        let actual_rate = self.device.sample_rate(Direction::Rx, channel)?;
        let actual_bw = self.device.bandwidth(Direction::Rx, channel)?;
        let actual_gain = self.device.gain(Direction::Rx, channel)?;
        let actual_ant = self.device.antenna(Direction::Rx, channel)?;
        info!(
            "setup_rx: actual antenna={} rate={} bw={} gain={}",
            actual_ant, actual_rate, actual_bw, actual_gain
        );
        Ok(self.device.rx_stream::<Complex32>(&[channel])?)
    }
}

fn apply_tx_gain(device: &soapysdr::Device, channel: usize, gain_db: f64) -> Result<f64, Error> {
    crate::stream::finite(gain_db, "TX gain")?;
    device.set_gain(soapysdr::Direction::Tx, channel, gain_db)?;
    Ok(device.gain(soapysdr::Direction::Tx, channel)?)
}

impl Radio for SoapySdrRadio {
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
        self.device.has_hardware_time(None).unwrap_or(false)
    }
    fn tick_rate(&self) -> u64 {
        1_000_000_000
    }

    fn set_tx_frequency(&mut self, center_frequency: usize) -> Result<(), Error> {
        crate::stream::positive_hz(center_frequency as f64, "set_tx_frequency")?;
        self.device.set_frequency(
            soapysdr::Direction::Tx,
            self.channel,
            center_frequency as f64 + self.tx_lo_offset_hz,
            "",
        )?;
        Ok(())
    }

    fn set_tx_sample_rate(&mut self, sample_rate_hz: f64) -> Result<f64, Error> {
        crate::stream::positive_hz(sample_rate_hz, "set_tx_sample_rate")?;
        self.device
            .set_sample_rate(soapysdr::Direction::Tx, self.channel, sample_rate_hz)?;
        self.tx_sample_rate_hz = self
            .device
            .sample_rate(soapysdr::Direction::Tx, self.channel)?;
        self.tx_nco_phase_rad = 0.0;
        Ok(self.tx_sample_rate_hz)
    }

    fn set_tx_bandwidth(&mut self, bandwidth: usize) -> Result<(), Error> {
        crate::stream::positive_hz(bandwidth as f64, "set_tx_bandwidth")?;
        self.device
            .set_bandwidth(soapysdr::Direction::Tx, self.channel, bandwidth as f64)?;
        Ok(())
    }

    fn set_tx_lo_offset_hz(&mut self, offset_hz: i64) -> Result<(), Error> {
        self.tx_lo_offset_hz = offset_hz as f64;
        self.tx_nco_phase_rad = 0.0;
        Ok(())
    }

    fn tx_gain_range_db(&self) -> Option<(f64, f64)> {
        let range = self
            .device
            .gain_range(soapysdr::Direction::Tx, self.channel)
            .ok()?;
        Some((range.minimum, range.maximum))
    }

    fn hardware_time(&self) -> Result<u64, Error> {
        u64::try_from(self.device.get_hardware_time(None)?)
            .map_err(|_| Error::invalid_setting("Soapy hardware time is negative"))
    }

    fn tx_gain_db(&self) -> Option<f64> {
        self.device.gain(soapysdr::Direction::Tx, self.channel).ok()
    }

    fn set_tx_gain(&mut self, gain_db: f64) -> Result<f64, Error> {
        apply_tx_gain(&self.device, self.channel, gain_db)
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
        self.rx_channel = channel;
        let stream =
            self.setup_rx_stream(channel, antenna, sample_rate_hz, bandwidth_hz, gain_db)?;
        self.rx_stream = Some(stream);
        self.rx_rate_hz = self.device.sample_rate(Direction::Rx, channel)?;
        Ok(self.rx_rate_hz)
    }

    fn split(self: Box<Self>) -> Result<(Box<dyn RadioTx>, Option<Box<dyn RadioRx>>), Error> {
        if self.tx_sample_rate_hz <= 0.0 {
            return Err("SoapySDR TX sample rate must be configured before split".into());
        }
        let device = std::sync::Arc::new(self.device);
        let tx = SoapyTxHalf {
            tx_scratch: Vec::new(),
            active: false,
            device: device.clone(),
            stream: self.stream,
            tx_lo_offset_hz: self.tx_lo_offset_hz,
            tx_sample_rate_hz: self.tx_sample_rate_hz,
            tx_nco_phase_rad: self.tx_nco_phase_rad,
            channel: self.channel,
            health: TxRadioHealth::default(),
            burst_pending: false,
        };
        let rx_channel = self.rx_channel;
        let rx = self.rx_stream.map(|s| -> Box<dyn RadioRx> {
            Box::new(SoapyRxHalf {
                rx_rate_hz: self.rx_rate_hz,
                device: device.clone(),
                stream: s,
                channel: rx_channel,
            })
        });
        Ok((Box::new(tx), rx))
    }
}

/// SoapySDR TX stream returned by [`Radio::split`]. Uses nanosecond timestamps.
struct SoapyTxHalf {
    tx_scratch: Vec<Complex32>,
    active: bool,
    device: std::sync::Arc<soapysdr::Device>,
    stream: TxStream<Complex32>,
    tx_lo_offset_hz: f64,
    tx_sample_rate_hz: f64,
    tx_nco_phase_rad: f64,
    channel: usize,
    health: TxRadioHealth,
    burst_pending: bool,
}

impl SoapyTxHalf {
    fn finish_transmit(&mut self, tick: Option<u64>) -> Result<(), Error> {
        let end_result = if std::mem::take(&mut self.burst_pending) {
            let silence = [Complex32::new(0.0, 0.0)];
            self.stream
                .write_all(&[&silence], None, true, TX_WRITE_TIMEOUT_US)
        } else {
            Ok(())
        };
        let deactivate_result = self.stream.deactivate(tick.map(|t| t as i64));
        deactivate_result?;
        self.active = false;
        end_result?;
        Ok(())
    }

    fn drain_tx_status(&mut self) {
        for _ in 0..MAX_TX_STATUS_EVENTS_PER_POLL {
            let mut chan_mask = 0usize;
            let mut flags = 0i32;
            let mut time_ns = 0i64;
            match self
                .stream
                .read_status(&mut chan_mask, &mut flags, &mut time_ns, 0)
            {
                Ok(_) => {
                    if flags & (SOAPY_END_BURST as i32) != 0 {
                        self.health.burst_acks += 1;
                    }
                }
                Err(err) => match err.code {
                    soapysdr::ErrorCode::Timeout => return,
                    soapysdr::ErrorCode::TimeError => {
                        self.health.late_packets += 1;
                        warn!(
                            "soapy tx: device discarded a burst scheduled for {time_ns} ns, \
                             its tick had already passed"
                        );
                    }
                    soapysdr::ErrorCode::Underflow => self.health.underflows += 1,
                    soapysdr::ErrorCode::Corruption => self.health.sequence_errors += 1,
                    soapysdr::ErrorCode::Overflow => self.health.dropped_packets += 1,
                    _ => self.health.unknown_events += 1,
                },
            }
        }
    }

    fn apply_tx_lo_offset(&mut self, samples: &mut [Complex32]) {
        if self.tx_lo_offset_hz == 0.0 || self.tx_sample_rate_hz <= 0.0 {
            return;
        }

        let phase_step =
            -2.0 * std::f64::consts::PI * self.tx_lo_offset_hz / self.tx_sample_rate_hz;
        let mut phase = self.tx_nco_phase_rad;

        for sample in samples.iter_mut() {
            let rot = Complex32::new(phase.cos() as f32, phase.sin() as f32);
            *sample *= rot;
            phase += phase_step;
        }

        self.tx_nco_phase_rad = phase.rem_euclid(2.0 * std::f64::consts::PI);
    }
}

impl RadioTx for SoapyTxHalf {
    fn capabilities(&self) -> crate::TxCapabilities {
        let capabilities = crate::TxCapabilities::from([crate::TxCapability::BurstEnd]);
        if self.supports_timed_tx() {
            capabilities.with(crate::TxCapability::TimedSamples)
        } else {
            capabilities
        }
    }
    fn prepare_transmit(&mut self, max_samples: usize) -> Result<(), Error> {
        self.tx_scratch
            .reserve(max_samples.saturating_sub(self.tx_scratch.len()));
        Ok(())
    }
    fn write(
        &mut self,
        samples: &[Complex32],
        metadata: crate::TxMetadata,
    ) -> Result<usize, Error> {
        let tick = metadata.schedule.ticks();
        if tick.is_some() && !self.supports_timed_tx() {
            return Err(Error::unsupported("timed TX"));
        }
        let tick = tick
            .map(i64::try_from)
            .transpose()
            .map_err(|_| Error::invalid_setting("Soapy timestamp exceeds i64"))?;
        let phase = self.tx_nco_phase_rad;
        let mut scratch = std::mem::take(&mut self.tx_scratch);
        scratch.clear();
        scratch.extend_from_slice(samples);
        self.apply_tx_lo_offset(&mut scratch);
        let result = self.stream.write(
            &[&scratch],
            tick,
            metadata.end_of_burst,
            TX_WRITE_TIMEOUT_US,
        );
        self.tx_scratch = scratch;
        match result {
            Ok(count) if count <= samples.len() => {
                self.tx_nco_phase_rad = phase
                    - std::f64::consts::TAU * self.tx_lo_offset_hz * count as f64
                        / self.tx_sample_rate_hz;
                self.burst_pending = !metadata.end_of_burst || count != samples.len();
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
        if self.burst_pending {
            let silence = [Complex32::default()];
            self.stream
                .write_all(&[&silence], None, true, TX_WRITE_TIMEOUT_US)?;
            self.burst_pending = false;
        }
        Ok(())
    }

    fn tx_sample_rate_hz(&self) -> f64 {
        self.tx_sample_rate_hz
    }
    fn supports_timed_tx(&self) -> bool {
        self.device.has_hardware_time(None).unwrap_or(false)
    }
    fn tick_rate(&self) -> u64 {
        1_000_000_000
    }

    fn get_hardware_time(&self) -> Result<u64, Error> {
        u64::try_from(self.device.get_hardware_time(None)?)
            .map_err(|_| Error::invalid_setting("Soapy hardware time is negative"))
    }

    fn set_hardware_time(&self, ticks: u64) -> Result<(), Error> {
        let ticks = i64::try_from(ticks)
            .map_err(|_| Error::invalid_setting("Soapy timestamp exceeds i64"))?;
        self.device.set_hardware_time(None, ticks)?;
        Ok(())
    }

    fn transmit(&mut self, samples: &[Complex32]) -> Result<(), Error> {
        self.transmit_at(samples, None)
    }

    fn tx_health(&mut self) -> Result<TxRadioHealth, Error> {
        self.drain_tx_status();
        Ok(self.health)
    }

    fn transmit_at(&mut self, samples: &[Complex32], tick: Option<u64>) -> Result<(), Error> {
        self.write_all(
            samples,
            crate::TxMetadata {
                schedule: tick.map_or(crate::Schedule::Immediate, |tick| {
                    crate::Schedule::At(crate::DeviceTime::from_ticks(tick))
                }),
                end_of_burst: false,
            },
        )
    }

    fn enable_transmit(&mut self, enable: bool) -> Result<(), Error> {
        if enable {
            self.tx_nco_phase_rad = 0.0;
            if !self.active {
                self.stream.activate(None)?;
                self.active = true;
            }
        } else {
            self.finish_transmit(None)?;
        }
        Ok(())
    }

    fn enable_transmit_at(&mut self, enable: bool, tick: Option<u64>) -> Result<(), Error> {
        if tick.is_some() {
            return Err(Error::unsupported("timed TX activation"));
        }
        self.enable_transmit(enable)
    }

    fn set_tx_frequency_hz(&mut self, frequency_hz: f64) -> Result<(), Error> {
        crate::stream::positive_hz(frequency_hz, "frequency")?;
        self.device.set_frequency(
            soapysdr::Direction::Tx,
            self.channel,
            frequency_hz + self.tx_lo_offset_hz,
            "",
        )?;
        Ok(())
    }

    fn set_tx_gain(&mut self, gain_db: f64) -> Result<f64, Error> {
        apply_tx_gain(&self.device, self.channel, gain_db)
    }
}

/// SoapySDR RX stream returned by [`Radio::split`] after RX configuration.
struct SoapyRxHalf {
    rx_rate_hz: f64,
    device: std::sync::Arc<soapysdr::Device>,
    stream: RxStream<Complex32>,
    channel: usize,
}

impl RadioRx for SoapyRxHalf {
    fn rx_sample_rate_hz(&self) -> f64 {
        self.rx_rate_hz
    }
    fn tick_rate(&self) -> u64 {
        1_000_000_000
    }

    fn get_hardware_time(&self) -> Result<u64, Error> {
        u64::try_from(self.device.get_hardware_time(None)?)
            .map_err(|_| Error::invalid_setting("Soapy hardware time is negative"))
    }

    fn rx_read(&mut self, buf: &mut [Complex32], timeout_us: i64) -> Result<RxReadResult, Error> {
        if timeout_us < 0 {
            return Err(Error::unsupported("unbounded RX wait"));
        }
        match self.stream.read(&mut [buf], timeout_us) {
            Ok(n) => {
                let time_ticks = self.stream.time_ns() as u64;
                Ok(RxReadResult {
                    samples_read: n,
                    time_ticks,
                    overflow: false,
                })
            }
            Err(e) if e.code == soapysdr::ErrorCode::Timeout => Ok(RxReadResult {
                samples_read: 0,
                time_ticks: 0,
                overflow: false,
            }),
            Err(e) if e.code == soapysdr::ErrorCode::Overflow => Ok(RxReadResult {
                samples_read: 0,
                time_ticks: 0,
                overflow: true,
            }),
            Err(e) => Err(e.into()),
        }
    }

    fn rx_activate(&mut self, time_ticks: Option<u64>) -> Result<(), Error> {
        let tick = time_ticks
            .map(i64::try_from)
            .transpose()
            .map_err(|_| Error::invalid_setting("RX timestamp exceeds i64"))?;
        self.stream.activate(tick)?;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), Error> {
        self.stream.deactivate(None)?;
        Ok(())
    }

    fn set_rx_frequency(&mut self, frequency_hz: f64) -> Result<(), Error> {
        crate::stream::positive_hz(frequency_hz, "frequency")?;
        self.device
            .set_frequency(Direction::Rx, self.channel, frequency_hz, "")?;
        Ok(())
    }

    fn set_rx_gain(&mut self, gain_db: f64) -> Result<(), Error> {
        crate::stream::finite(gain_db, "gain")?;
        self.device.set_gain(Direction::Rx, self.channel, gain_db)?;
        Ok(())
    }
}
