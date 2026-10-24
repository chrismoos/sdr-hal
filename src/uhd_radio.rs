use std::sync::Arc;

use parking_lot::Mutex;

use crate::error::Error;
use log::{debug, info};
use num_complex::Complex32;
use uhd::{
    ReceiveStreamer, StreamArgs, StreamCommand, StreamCommandType, StreamTime, TimeSpec,
    TransmitMetadata, TransmitStreamer, TuneRequest, Usrp,
};

use super::{Radio, RadioRx, RadioTx, RxReadResult, TxRadioHealth};

mod tx_correction;
use tx_correction::TxFrequencyCorrection;

/// Callers read the clock from a streaming hot path, so time reads must not wait
/// on a lock. UHD does not allow concurrent settings changes, so those hold
/// `settings`.
struct SharedUsrp {
    usrp: Usrp,
    settings: Mutex<()>,
}

impl SharedUsrp {
    fn current_ticks(&self, master_clock_rate: u64) -> Result<u64, Error> {
        let ts = self.usrp.get_current_time(0).map_err(|e| {
            let context = format!("UHD: get_current_time: {}", e);
            Error::from(e).context(context)
        })?;
        Ok(timespec_to_ticks(&ts, master_clock_rate))
    }

    fn configure<T>(&self, change: impl FnOnce(&Usrp) -> T) -> T {
        let _settings = self.settings.lock();
        change(&self.usrp)
    }
}

fn record_async_tx_event(health: &mut TxRadioHealth, code: u32) {
    const BURST_ACK: u32 = 0x01;
    const UNDERFLOW: u32 = 0x02;
    const SEQUENCE_ERROR: u32 = 0x04;
    const TIME_ERROR: u32 = 0x08;
    const UNDERFLOW_IN_PACKET: u32 = 0x10;
    const SEQUENCE_ERROR_IN_BURST: u32 = 0x20;
    const USER_PAYLOAD: u32 = 0x40;
    const KNOWN: u32 = BURST_ACK
        | UNDERFLOW
        | SEQUENCE_ERROR
        | TIME_ERROR
        | UNDERFLOW_IN_PACKET
        | SEQUENCE_ERROR_IN_BURST
        | USER_PAYLOAD;

    health.burst_acks += u64::from(code & BURST_ACK != 0);
    health.underflows += u64::from(code & (UNDERFLOW | UNDERFLOW_IN_PACKET) != 0);
    health.sequence_errors += u64::from(code & (SEQUENCE_ERROR | SEQUENCE_ERROR_IN_BURST) != 0);
    health.late_packets += u64::from(code & TIME_ERROR != 0);
    health.unknown_events += u64::from(code & (USER_PAYLOAD | !KNOWN) != 0);
}

fn ticks_to_timespec(ticks: u64, tick_rate: u64) -> TimeSpec {
    let full_secs = (ticks / tick_rate) as i64;
    let frac_ticks = ticks % tick_rate;
    let frac_secs = frac_ticks as f64 / tick_rate as f64;
    TimeSpec {
        seconds: full_secs,
        fraction: frac_secs,
    }
}

fn timespec_to_ticks(ts: &TimeSpec, tick_rate: u64) -> u64 {
    let rate_i = tick_rate as i64;
    let ticks_full = ts.seconds * rate_i;
    let ticks_frac = (ts.fraction * tick_rate as f64).round() as i64;
    (ticks_full + ticks_frac) as u64
}

const NANOS_PER_SECOND: f64 = 1e9;

/// Ticks per sample within this of a whole number count as aligned.
const TICK_ALIGNMENT_TOLERANCE: f64 = 1e-6;

fn log_tx_tick_alignment(master_clock_rate: u64, tx_sample_rate_hz: f64) {
    let ticks_per_sample = master_clock_rate as f64 / tx_sample_rate_hz;
    let whole = ticks_per_sample.round();
    if whole >= 1.0 && (ticks_per_sample - whole).abs() < TICK_ALIGNMENT_TOLERANCE {
        info!("UHD: tick alignment: {master_clock_rate}/{tx_sample_rate_hz}={whole} ticks/sample");
    } else {
        log::warn!(
            "UHD: master_clock_rate {master_clock_rate} is not aligned to TX sample rate {tx_sample_rate_hz}, {ticks_per_sample} ticks/sample"
        );
    }
}

/// Doublings tried when discovering how high the device clock goes.
const CLOCK_CEILING_DOUBLINGS: usize = 4;

/// Reads and writes a USRP master clock for [`crate::clock::select_device_clock`].
/// UHD exposes no master-clock range, so a ceiling has to be found by asking.
struct UhdClockProbe<'a> {
    usrp: &'a mut Usrp,
    max_clock_hz: Option<u64>,
}

impl crate::clock::ClockProbe for UhdClockProbe<'_> {
    fn current_hz(&mut self) -> Result<u64, Error> {
        let clock = self.usrp.get_master_clock_rate(0).map_err(|e| {
            let context = format!("UHD: get master clock rate: {e}");
            Error::from(e).context(context)
        })?;
        Ok(clock.round() as u64)
    }

    fn ceiling_hz(&mut self) -> Result<u64, Error> {
        if let Some(max) = self.max_clock_hz {
            return Ok(max);
        }
        let anchor = self.current_hz()?;
        let mut ceiling = anchor;
        let mut candidate = anchor;
        for _ in 0..CLOCK_CEILING_DOUBLINGS {
            candidate = candidate.saturating_mul(2);
            let applied = self.try_clock(candidate)?;
            ceiling = ceiling.max(applied);
            if applied < candidate {
                break;
            }
        }
        debug!("UHD: master clock ceiling discovered as {ceiling} Hz from {anchor} Hz");
        Ok(ceiling)
    }

    fn try_clock(&mut self, candidate_hz: u64) -> Result<u64, Error> {
        // A refused value is a candidate the device will not run, not a failure. The
        // readback below reports where the device actually is either way.
        if let Err(e) = self.usrp.set_master_clock_rate(candidate_hz as f64, 0) {
            debug!("UHD: master clock {candidate_hz} Hz refused: {e}");
        }
        self.current_hz()
    }
}

/// A USRP radio with TX and optional RX. Both streams use the device master clock for timestamps.
/// Configure RX and clock sources before splitting. Native RX activation starts immediately.
pub struct UhdRadio {
    tx_rate_hz: f64,
    rx_rate_hz: f64,
    usrp: Usrp,
    channel: usize,
    rx_channel: usize,
    master_clock_rate: u64,
    /// A configured clock is applied as given. Without one the backend selects a clock
    /// that divides every requested rate.
    clock_pinned: bool,
    max_clock_hz: Option<u64>,
    /// Clock selection works from what the caller asked for, not from the applied rate.
    /// A driver can coerce a rate to a fraction of a Hz, which no clock divides.
    tx_rate_request_hz: Option<f64>,
    rx_rate_request_hz: Option<f64>,
    tx_lo_offset_hz: f64,
    tx_correction: TxFrequencyCorrection,
    tx_streamer: Option<TransmitStreamer<Complex32>>,
    rx_streamer: Option<ReceiveStreamer<Complex32>>,
}

impl UhdRadio {
    /// Opens a USRP selected by `device_args`. Channel numbers start at zero.
    /// Supply the TX port name and gain in dB. An absent sample rate opens the device
    /// without configuring TX, for a caller that negotiates the rate later. An absent
    /// `master_clock_rate` selects a clock that divides every rate as rates arrive.
    /// The driver can round rates, so use the applied rates when calculating timestamps.
    pub fn new(
        device_args: &str,
        channel: usize,
        tx_antenna: &str,
        tx_gain_db: f64,
        master_clock_rate: Option<u64>,
        max_master_clock_rate: Option<u64>,
        tx_sample_rate_hz: Option<usize>,
    ) -> Result<UhdRadio, Error> {
        if let Some(rate) = tx_sample_rate_hz {
            crate::stream::positive_hz(rate as f64, "TX sample rate")?;
        }
        crate::stream::finite(tx_gain_db, "TX gain")?;
        if let Some(mcr) = master_clock_rate {
            crate::stream::positive_hz(mcr as f64, "master clock rate")?;
        }
        let mut usrp = Usrp::open(device_args).map_err(|e| {
            let context = format!("UHD: failed to open device: {}", e);
            Error::from(e).context(context)
        })?;

        info!("UHD: device opened, args={}", device_args);

        // Rate negotiation depends on the master clock, so configure it first.
        if let Some(mcr) = master_clock_rate {
            usrp.set_master_clock_rate(mcr as f64, 0).map_err(|e| {
                let context = format!("UHD: set master clock rate: {}", e);
                Error::from(e).context(context)
            })?;
        }

        let actual_mcr = usrp.get_master_clock_rate(0).map_err(|e| {
            let context = format!("UHD: get master clock rate: {}", e);
            Error::from(e).context(context)
        })?;
        match master_clock_rate {
            Some(mcr) => info!("UHD: master_clock_rate requested={mcr} actual={actual_mcr}"),
            None => info!(
                "UHD: master_clock_rate not configured, device default={actual_mcr}, selecting one per sample rate"
            ),
        }

        if let Some(rate) = tx_sample_rate_hz {
            log_tx_tick_alignment(actual_mcr.round() as u64, rate as f64);
        }

        usrp.set_tx_antenna(tx_antenna, channel).map_err(|e| {
            let context = format!("UHD: set TX antenna: {}", e);
            Error::from(e).context(context)
        })?;
        usrp.set_tx_gain(tx_gain_db, channel, "").map_err(|e| {
            let context = format!("UHD: set TX gain: {}", e);
            Error::from(e).context(context)
        })?;

        let mb_name = usrp.get_motherboard_name(0).unwrap_or_default();
        info!(
            "UHD: motherboard={} channel={} antenna={} gain={}dB",
            mb_name, channel, tx_antenna, tx_gain_db
        );

        let mut radio = UhdRadio {
            tx_rate_hz: 0.0,
            rx_rate_hz: 0.0,
            usrp,
            channel,
            rx_channel: channel,
            master_clock_rate: actual_mcr.round() as u64,
            clock_pinned: master_clock_rate.is_some(),
            max_clock_hz: max_master_clock_rate,
            tx_rate_request_hz: None,
            rx_rate_request_hz: None,
            tx_lo_offset_hz: 0.0,
            tx_correction: TxFrequencyCorrection::default(),
            tx_streamer: None,
            rx_streamer: None,
        };
        match tx_sample_rate_hz {
            Some(rate) => {
                crate::Radio::set_tx_sample_rate(&mut radio, rate as f64)?;
            }
            None => {
                radio.refresh_clock()?;
            }
        }
        Ok(radio)
    }

    /// Select before tuning. Software correction requires timed TX and zero LO offset.
    pub fn with_software_tx_frequency_correction(mut self, enabled: bool) -> Self {
        self.tx_correction = if enabled {
            TxFrequencyCorrection::software()
        } else {
            TxFrequencyCorrection::Hardware
        };
        self
    }

    /// Re-reads the master clock and the applied TX rate. UHD moves the master clock
    /// itself to reach a requested rate, so the stored clock goes stale the moment a rate
    /// is applied, and it is the divisor behind every timestamp.
    fn refresh_clock(&mut self) -> Result<u64, Error> {
        let clock = self.usrp.get_master_clock_rate(0).map_err(|e| {
            let context = format!("UHD: get master clock rate: {e}");
            Error::from(e).context(context)
        })?;
        crate::stream::positive_hz(clock, "master clock rate")?;
        self.master_clock_rate = clock.round() as u64;
        self.tx_rate_hz = self.usrp.get_tx_sample_rate(self.channel).map_err(|e| {
            let context = format!("UHD: read TX rate: {e}");
            Error::from(e).context(context)
        })?;
        Ok(self.master_clock_rate)
    }

    fn requested_rates_hz(&self) -> Vec<f64> {
        let mut rates = Vec::new();
        if let Some(rate) = self.tx_rate_request_hz {
            rates.push(rate);
        }
        if let Some(rate) = self.rx_rate_request_hz {
            rates.push(rate);
        }
        rates
    }

    /// Applies a clock that divides every requested rate. Does nothing when the caller
    /// pinned a clock or when no rate has been requested yet.
    fn select_clock(&mut self) -> Result<(), Error> {
        if self.clock_pinned {
            return Ok(());
        }
        let rates = self.requested_rates_hz();
        if rates.is_empty() {
            return Ok(());
        }
        let mut probe = UhdClockProbe {
            usrp: &mut self.usrp,
            max_clock_hz: self.max_clock_hz,
        };
        let clock = crate::clock::select_device_clock(&mut probe, &rates)?;
        self.master_clock_rate = clock;
        info!("UHD: selected master_clock_rate={clock} for rates {rates:?}");
        Ok(())
    }

    /// Set clock source (e.g. "internal", "external", "gpsdo").
    pub fn set_clock_source(&self, source: &str) -> Result<(), Error> {
        self.usrp.set_clock_source(source, 0).map_err(|e| {
            let context = format!("UHD: set clock source: {}", e);
            Error::from(e).context(context)
        })?;
        info!("UHD: clock source set to '{}'", source);
        Ok(())
    }

    /// Selects the device time source, such as `internal`, `external`, or `gpsdo`.
    /// This does not itself reset hardware time.
    pub fn set_time_source(&self, source: &str) -> Result<(), Error> {
        self.usrp.set_time_source(source, 0).map_err(|e| {
            let context = format!("UHD: time source: {}", e);
            Error::from(e).context(context)
        })?;
        info!("UHD: time source set to '{}'", source);
        Ok(())
    }
}

/// Applies a TX gain and reads back what the device settled on.
fn apply_tx_gain(usrp: &Usrp, gain_db: f64, channel: usize) -> Result<f64, Error> {
    crate::stream::finite(gain_db, "TX gain")?;
    usrp.set_tx_gain(gain_db, channel, "").map_err(|e| {
        let context = format!("UHD: set TX gain: {e}");
        Error::from(e).context(context)
    })?;
    usrp.get_tx_gain(channel, "").map_err(|e| {
        let context = format!("UHD: read TX gain: {e}");
        Error::from(e).context(context)
    })
}

impl Radio for UhdRadio {
    fn tx_capabilities(&self) -> crate::TxCapabilities {
        let capabilities = crate::TxCapabilities::from([
            crate::TxCapability::BurstEnd,
            crate::TxCapability::ClockReset,
        ]);
        if self.supports_timed_tx() {
            capabilities.with(crate::TxCapability::TimedSamples)
        } else {
            capabilities
        }
    }

    fn tx_sample_rate_hz(&self) -> f64 {
        self.tx_rate_hz
    }
    fn rx_sample_rate_hz(&self) -> Option<f64> {
        self.rx_streamer.as_ref().map(|_| self.rx_rate_hz)
    }
    fn supports_timed_tx(&self) -> bool {
        true
    }
    fn tick_rate(&self) -> u64 {
        self.master_clock_rate
    }

    fn set_tx_frequency(&mut self, center_frequency: usize) -> Result<(), Error> {
        crate::stream::positive_hz(center_frequency as f64, "set_tx_frequency")?;
        let request = self
            .tx_correction
            .tune_request(center_frequency as f64, self.tx_lo_offset_hz)?;
        let result = self
            .usrp
            .set_tx_frequency(&request, self.channel)
            .map_err(|e| {
                let context = format!("UHD: set TX freq: {}", e);
                Error::from(e).context(context)
            })?;
        self.tx_correction.update_tune(
            center_frequency as f64,
            result.actual_rf_freq(),
            result.actual_dsp_freq(),
        )?;
        debug!(
            "UHD: TX tune target_rf={} actual_rf={} dsp={}",
            result.target_rf_freq(),
            result.actual_rf_freq(),
            result.actual_dsp_freq()
        );
        Ok(())
    }

    fn set_tick_rate(&mut self, tick_rate: u64) -> Result<u64, Error> {
        crate::stream::positive_hz(tick_rate as f64, "master clock rate")?;
        self.usrp
            .set_master_clock_rate(tick_rate as f64, 0)
            .map_err(|e| {
                let context = format!("UHD: set master clock rate: {}", e);
                Error::from(e).context(context)
            })?;
        // An explicit clock wins over selection for the rest of this radio's life.
        self.clock_pinned = true;
        self.refresh_clock()?;
        info!(
            "UHD: master_clock_rate requested={} actual={} tx_rate={}",
            tick_rate, self.master_clock_rate, self.tx_rate_hz
        );
        Ok(self.master_clock_rate)
    }
    fn set_tx_sample_rate(&mut self, sample_rate_hz: f64) -> Result<f64, Error> {
        crate::stream::positive_hz(sample_rate_hz, "set_tx_sample_rate")?;
        self.tx_rate_request_hz = Some(sample_rate_hz);
        // The clock decides which rates are reachable, so choose it before asking.
        self.select_clock()?;
        self.usrp
            .set_tx_sample_rate(sample_rate_hz, self.channel)
            .map_err(|e| {
                let context = format!("UHD: set TX rate: {}", e);
                Error::from(e).context(context)
            })?;
        self.refresh_clock()?;
        log_tx_tick_alignment(self.master_clock_rate, self.tx_rate_hz);
        Ok(self.tx_rate_hz)
    }

    fn set_tx_bandwidth(&mut self, bandwidth: usize) -> Result<(), Error> {
        crate::stream::positive_hz(bandwidth as f64, "set_tx_bandwidth")?;
        self.usrp
            .set_tx_bandwidth(bandwidth as f64, self.channel)
            .map_err(|e| {
                let context = format!("UHD: set TX bandwidth: {}", e);
                Error::from(e).context(context)
            })?;
        Ok(())
    }

    fn set_tx_lo_offset_hz(&mut self, offset_hz: i64) -> Result<(), Error> {
        if self.tx_correction.is_software() && offset_hz != 0 {
            return Err(Error::invalid_setting(
                "software UHD TX frequency correction requires zero TX LO offset",
            ));
        }
        self.tx_lo_offset_hz = offset_hz as f64;
        Ok(())
    }

    fn tx_gain_range_db(&self) -> Option<(f64, f64)> {
        let range = self.usrp.get_tx_gain_range(self.channel, "").ok()?;
        Some((range.start().ok()?, range.stop().ok()?))
    }

    fn tx_gain_db(&self) -> Option<f64> {
        self.usrp.get_tx_gain(self.channel, "").ok()
    }

    fn hardware_time(&self) -> Result<u64, Error> {
        let ts = self.usrp.get_current_time(0).map_err(|e| {
            let context = format!("UHD: get_current_time: {e}");
            Error::from(e).context(context)
        })?;
        Ok(timespec_to_ticks(&ts, self.master_clock_rate))
    }

    fn set_tx_gain(&mut self, gain_db: f64) -> Result<f64, Error> {
        apply_tx_gain(&self.usrp, gain_db, self.channel)
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
        self.rx_rate_request_hz = Some(sample_rate_hz);
        // A clock chosen for TX alone need not divide this rate, so choose again now that
        // both are known, then put the TX rate back against the new clock.
        let reapply_tx = !self.clock_pinned && self.tx_rate_request_hz.is_some();
        self.select_clock()?;
        if reapply_tx {
            let tx_request = self.tx_rate_request_hz.expect("checked above");
            self.usrp
                .set_tx_sample_rate(tx_request, self.channel)
                .map_err(|e| {
                    let context = format!("UHD: set TX rate: {}", e);
                    Error::from(e).context(context)
                })?;
            self.refresh_clock()?;
            log_tx_tick_alignment(self.master_clock_rate, self.tx_rate_hz);
        }
        self.usrp.set_rx_antenna(antenna, channel).map_err(|e| {
            let context = format!("UHD: set RX antenna: {}", e);
            Error::from(e).context(context)
        })?;
        self.usrp
            .set_rx_sample_rate(sample_rate_hz, channel)
            .map_err(|e| {
                let context = format!("UHD: set RX rate: {}", e);
                Error::from(e).context(context)
            })?;
        self.usrp
            .set_rx_bandwidth(bandwidth_hz, channel)
            .map_err(|e| {
                let context = format!("UHD: set RX bandwidth: {}", e);
                Error::from(e).context(context)
            })?;
        if let Some(gain) = gain_db {
            self.usrp.set_rx_gain(gain, channel, "").map_err(|e| {
                let context = format!("UHD: set RX gain: {}", e);
                Error::from(e).context(context)
            })?;
        }
        let rx_streamer = self
            .usrp
            .get_rx_stream(
                &StreamArgs::<Complex32>::builder()
                    .channels(vec![channel])
                    .build(),
            )
            .map_err(|e| {
                let context = format!("UHD: create RX stream: {}", e);
                Error::from(e).context(context)
            })?;
        self.rx_streamer = Some(rx_streamer);
        info!(
            "UHD: RX configured antenna={} rate={} bw={} gain={:?}",
            antenna, sample_rate_hz, bandwidth_hz, gain_db
        );
        self.rx_rate_hz = self.usrp.get_rx_sample_rate(channel).map_err(|e| {
            let context = format!("UHD: read RX rate: {e}");
            Error::from(e).context(context)
        })?;
        Ok(self.rx_rate_hz)
    }

    fn split(self: Box<Self>) -> Result<(Box<dyn RadioTx>, Option<Box<dyn RadioRx>>), Error> {
        let mut this = *self;
        // The TX streamer is built here, not at open, because selecting the device clock
        // during rate negotiation retunes the converters and invalidates any streamer
        // that already exists.
        let tx_streamer = match this.tx_streamer.take() {
            Some(streamer) => streamer,
            None => this
                .usrp
                .get_tx_stream(
                    &StreamArgs::<Complex32>::builder()
                        .channels(vec![this.channel])
                        .build(),
                )
                .map_err(|e| {
                    let context = format!("UHD: create TX stream: {e}");
                    Error::from(e).context(context)
                })?,
        };
        let usrp = Arc::new(SharedUsrp {
            usrp: this.usrp,
            settings: Mutex::new(()),
        });
        let tx = UhdTxHalf {
            tx_rate_hz: this.tx_rate_hz,
            usrp: usrp.clone(),
            tx_streamer,
            master_clock_rate: this.master_clock_rate,
            start_of_burst: true,
            health: TxRadioHealth::default(),
            channel: this.channel,
            tx_lo_offset_hz: this.tx_lo_offset_hz,
            tx_correction: this.tx_correction,
        };
        let rx_channel = this.rx_channel;
        let rx = this.rx_streamer.map(|s| -> Box<dyn RadioRx> {
            Box::new(UhdRxHalf {
                rx_rate_hz: this.rx_rate_hz,
                usrp: usrp.clone(),
                rx_streamer: s,
                master_clock_rate: this.master_clock_rate,
                channel: rx_channel,
            })
        });
        Ok((Box::new(tx), rx))
    }
}

struct UhdTxHalf {
    tx_rate_hz: f64,
    usrp: Arc<SharedUsrp>,
    tx_streamer: TransmitStreamer<Complex32>,
    master_clock_rate: u64,
    start_of_burst: bool,
    health: TxRadioHealth,
    channel: usize,
    tx_lo_offset_hz: f64,
    tx_correction: TxFrequencyCorrection,
}

struct UhdClockReader {
    usrp: Arc<SharedUsrp>,
    tick_rate: u64,
}

impl crate::RadioClock for UhdClockReader {
    fn get_hardware_time(&self) -> Result<u64, Error> {
        self.usrp.current_ticks(self.tick_rate)
    }
}

impl RadioTx for UhdTxHalf {
    fn clock_reader(&self) -> Option<Box<dyn crate::RadioClock>> {
        Some(Box::new(UhdClockReader {
            usrp: Arc::clone(&self.usrp),
            tick_rate: self.master_clock_rate,
        }))
    }

    fn capabilities(&self) -> crate::TxCapabilities {
        crate::TxCapabilities::from([
            crate::TxCapability::TimedSamples,
            crate::TxCapability::ClockReset,
            crate::TxCapability::BurstEnd,
        ])
    }
    fn write(
        &mut self,
        samples: &[Complex32],
        metadata: crate::TxMetadata,
    ) -> Result<usize, Error> {
        let native = match metadata.schedule {
            crate::Schedule::At(time) => {
                let ts = ticks_to_timespec(time.ticks(), self.master_clock_rate);
                TransmitMetadata::with_time(
                    ts.seconds,
                    ts.fraction,
                    self.start_of_burst,
                    metadata.end_of_burst,
                )?
            }
            crate::Schedule::Immediate => {
                TransmitMetadata::with_flags(self.start_of_burst, metadata.end_of_burst)?
            }
        };
        let corrected = self.tx_correction.samples(
            samples,
            metadata.schedule,
            self.tx_rate_hz,
            self.master_clock_rate,
        )?;
        let count = self
            .tx_streamer
            .send_with_metadata(&mut [corrected], &native, 1.0)?;
        if count > samples.len() {
            return Err(Error::incomplete_write(count, samples.len()));
        }
        if count != 0 {
            self.start_of_burst = metadata.end_of_burst && count == samples.len();
        }
        Ok(count)
    }
    fn end_burst(&mut self) -> Result<(), Error> {
        let metadata = TransmitMetadata::with_flags(false, true)?;
        let empty: &[Complex32] = &[];
        let result = self
            .tx_streamer
            .send_with_metadata(&mut [empty], &metadata, 0.1);
        finish_tx_send(result, 0, &mut self.start_of_burst)?;
        self.start_of_burst = true;
        Ok(())
    }

    fn tx_sample_rate_hz(&self) -> f64 {
        self.tx_rate_hz
    }
    fn supports_timed_tx(&self) -> bool {
        true
    }
    fn tick_rate(&self) -> u64 {
        self.master_clock_rate
    }

    fn get_hardware_time(&self) -> Result<u64, Error> {
        self.usrp.current_ticks(self.master_clock_rate)
    }

    fn set_hardware_time(&self, ticks: u64) -> Result<(), Error> {
        let ts = ticks_to_timespec(ticks, self.master_clock_rate);
        self.usrp
            .configure(|usrp| usrp.set_time_unknown_pps(ts.seconds, ts.fraction))
            .map_err(|e| {
                let context = format!("UHD: set_time_unknown_pps: {}", e);
                Error::from(e).context(context)
            })?;
        Ok(())
    }

    fn transmit(&mut self, samples: &[Complex32]) -> Result<(), Error> {
        self.transmit_at(samples, None)
    }

    fn prepare_transmit(&mut self, max_samples: usize) -> Result<(), Error> {
        self.tx_correction.prepare(max_samples);
        if let Some(event) = self.tx_streamer.recv_async_msg(0.0).map_err(|e| {
            let context = format!("UHD: prepare TX async metadata: {e}");
            Error::from(e).context(context)
        })? {
            record_async_tx_event(&mut self.health, event.code);
        }
        Ok(())
    }

    fn transmit_at(&mut self, samples: &[Complex32], tick: Option<u64>) -> Result<(), Error> {
        if self.tx_correction.is_software() {
            return self.write_all(
                samples,
                crate::TxMetadata {
                    schedule: tick.map_or(crate::Schedule::Immediate, |ticks| {
                        crate::Schedule::At(crate::DeviceTime::from_ticks(ticks))
                    }),
                    end_of_burst: false,
                },
            );
        }
        self.send_samples(samples, tick)
    }

    fn enable_transmit(&mut self, enable: bool) -> Result<(), Error> {
        self.enable_transmit_at(enable, None)
    }

    fn enable_transmit_at(&mut self, enable: bool, tick: Option<u64>) -> Result<(), Error> {
        if tick.is_some() {
            return Err(Error::unsupported("timed TX activation"));
        }
        if enable {
            self.start_of_burst = true;
        } else {
            self.end_burst()?;
        }
        Ok(())
    }

    fn tx_health(&mut self) -> Result<TxRadioHealth, Error> {
        // Keep event collection on the stream owner and cap each nonblocking
        // drain so diagnostics cannot delay a timed write indefinitely.
        for _ in 0..256 {
            let Some(event) = self.tx_streamer.recv_async_msg(0.0).map_err(|e| {
                let context = format!("UHD: TX async metadata: {e}");
                Error::from(e).context(context)
            })?
            else {
                break;
            };
            record_async_tx_event(&mut self.health, event.code);
        }
        Ok(self.health)
    }

    fn set_tx_frequency_hz(&mut self, frequency_hz: f64) -> Result<(), Error> {
        crate::stream::positive_hz(frequency_hz, "frequency")?;
        let request = self
            .tx_correction
            .tune_request(frequency_hz, self.tx_lo_offset_hz)?;
        let result = self
            .usrp
            .configure(|usrp| usrp.set_tx_frequency(&request, self.channel))
            .map_err(|e| {
                let context = format!("UHD: set TX freq: {}", e);
                Error::from(e).context(context)
            })?;
        self.tx_correction.update_tune(
            frequency_hz,
            result.actual_rf_freq(),
            result.actual_dsp_freq(),
        )?;
        Ok(())
    }

    fn set_tx_gain(&mut self, gain_db: f64) -> Result<f64, Error> {
        self.usrp
            .configure(|usrp| apply_tx_gain(usrp, gain_db, self.channel))
    }
}

impl UhdTxHalf {
    fn send_samples(&mut self, samples: &[Complex32], tick: Option<u64>) -> Result<(), Error> {
        let metadata = match tick {
            Some(t) => {
                let ts = ticks_to_timespec(t, self.master_clock_rate);
                TransmitMetadata::with_time(ts.seconds, ts.fraction, self.start_of_burst, false)
                    .map_err(|e| {
                        let context = format!("UHD: TX metadata: {}", e);
                        Error::from(e).context(context)
                    })?
            }
            None => TransmitMetadata::new().map_err(|e| {
                let context = format!("UHD: TX metadata: {}", e);
                Error::from(e).context(context)
            })?,
        };

        let result = self
            .tx_streamer
            .send_with_metadata(&mut [samples], &metadata, 1.0);
        finish_tx_send(result, samples.len(), &mut self.start_of_burst)
    }
}

fn finish_tx_send(
    result: Result<usize, uhd::Error>,
    requested_samples: usize,
    start_of_burst: &mut bool,
) -> Result<(), Error> {
    let sent_samples = result.map_err(|error| {
        let context = format!("UHD: TX send: {error}");
        Error::from(error).context(context)
    })?;
    if sent_samples != requested_samples {
        return Err(Error::incomplete_write(sent_samples, requested_samples));
    }
    *start_of_burst = false;
    Ok(())
}

struct UhdRxHalf {
    rx_rate_hz: f64,
    usrp: Arc<SharedUsrp>,
    rx_streamer: ReceiveStreamer<Complex32>,
    master_clock_rate: u64,
    channel: usize,
}

impl RadioRx for UhdRxHalf {
    fn rx_sample_rate_hz(&self) -> f64 {
        self.rx_rate_hz
    }
    fn tick_rate(&self) -> u64 {
        self.master_clock_rate
    }

    fn get_hardware_time(&self) -> Result<u64, Error> {
        self.usrp.current_ticks(self.master_clock_rate)
    }

    fn rx_read(&mut self, buf: &mut [Complex32], timeout_us: i64) -> Result<RxReadResult, Error> {
        if timeout_us < 0 {
            return Err(Error::unsupported("unbounded RX wait"));
        }
        let timeout_s = timeout_us as f64 / 1_000_000.0;
        let md = self
            .rx_streamer
            .receive(&mut [buf], timeout_s, false)
            .map_err(|e| {
                let context = format!("UHD: RX recv: {}", e);
                Error::from(e).context(context)
            })?;

        // Overflow metadata timestamps the resumed samples, allowing callers to measure the gap.
        let time_ticks = md
            .time_spec()
            .map_err(|e| {
                let context = format!("UHD: RX metadata: {}", e);
                Error::from(e).context(context)
            })?
            .map(|ts| timespec_to_ticks(&ts, self.master_clock_rate))
            .unwrap_or(0);

        if let Some(err) = md.last_error().map_err(|e| {
            let context = format!("UHD: RX metadata: {}", e);
            Error::from(e).context(context)
        })? {
            use uhd::ReceiveErrorKind;
            match err.kind() {
                ReceiveErrorKind::Timeout => {
                    return Ok(RxReadResult {
                        samples_read: 0,
                        time_ticks: 0,
                        overflow: false,
                    });
                }
                ReceiveErrorKind::Overflow => {
                    return Ok(RxReadResult {
                        samples_read: md.samples(),
                        time_ticks,
                        overflow: true,
                    });
                }
                _ => {
                    log::warn!("UHD: RX error: {:?}", err);
                }
            }
        }

        Ok(RxReadResult {
            samples_read: md.samples(),
            time_ticks,
            overflow: false,
        })
    }

    fn rx_activate(&mut self, time_ticks: Option<u64>) -> Result<(), Error> {
        let time = match time_ticks {
            None => StreamTime::Now,
            Some(ticks) => {
                // `Later` names an absolute device time, not a delay.
                let ts = ticks_to_timespec(ticks, self.master_clock_rate);
                let nanos = (ts.fraction * NANOS_PER_SECOND).round() as u32;
                StreamTime::Later(std::time::Duration::new(ts.seconds as u64, nanos))
            }
        };
        self.rx_streamer
            .send_command(&StreamCommand {
                time,
                command_type: StreamCommandType::StartContinuous,
            })
            .map_err(|e| {
                let context = format!("UHD: RX activate: {}", e);
                Error::from(e).context(context)
            })?;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), Error> {
        self.rx_streamer
            .send_command(&StreamCommand {
                time: StreamTime::Now,
                command_type: StreamCommandType::StopContinuous,
            })
            .map_err(|e| {
                let context = format!("UHD: RX deactivate: {}", e);
                Error::from(e).context(context)
            })?;
        Ok(())
    }

    fn set_rx_frequency(&mut self, frequency_hz: f64) -> Result<(), Error> {
        crate::stream::positive_hz(frequency_hz, "frequency")?;
        let tune = TuneRequest::with_frequency(frequency_hz);
        self.usrp
            .configure(|usrp| usrp.set_rx_frequency(&tune, self.channel))
            .map_err(|e| {
                let context = format!("UHD: set RX freq: {}", e);
                Error::from(e).context(context)
            })?;
        Ok(())
    }

    fn set_rx_gain(&mut self, gain_db: f64) -> Result<(), Error> {
        crate::stream::finite(gain_db, "gain")?;
        self.usrp
            .configure(|usrp| usrp.set_rx_gain(gain_db, self.channel, ""))
            .map_err(|e| {
                let context = format!("UHD: set RX gain: {}", e);
                Error::from(e).context(context)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{TxRadioHealth, finish_tx_send, record_async_tx_event};

    #[test]
    fn complete_data_and_empty_stop_sends_commit_the_burst_flag() {
        for count in [0, 10] {
            let mut start = true;
            finish_tx_send(Ok(count), count, &mut start).unwrap();
            assert!(!start);
        }
    }

    #[test]
    fn short_or_invalid_send_counts_are_errors_and_preserve_burst_state() {
        for (sent, requested) in [(0, 10), (5, 10), (11, 10), (1, 0)] {
            for initial in [false, true] {
                let mut start = initial;
                let error = finish_tx_send(Ok(sent), requested, &mut start).unwrap_err();
                assert!(error.to_string().contains("incomplete TX write"));
                assert_eq!(start, initial);
            }
        }
    }

    #[test]
    fn failed_stop_or_data_send_returns_the_native_error_without_committing_state() {
        for requested in [0, 10] {
            let mut start = true;
            let error = finish_tx_send(Err(uhd::Error::Usb), requested, &mut start).unwrap_err();
            assert!(error.to_string().contains("USB communication problem"));
            assert!(start);
        }
    }

    #[test]
    fn async_tx_events_are_classified_without_double_counting_variants() {
        let mut health = TxRadioHealth::default();
        record_async_tx_event(&mut health, 0x10);
        record_async_tx_event(&mut health, 0x08 | 0x20);
        record_async_tx_event(&mut health, 0x40);

        assert_eq!(health.underflows, 1);
        assert_eq!(health.late_packets, 1);
        assert_eq!(health.sequence_errors, 1);
        assert_eq!(health.unknown_events, 1);
    }
}
