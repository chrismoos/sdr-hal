//! One SoapySDR device backed by an `sdr-radiod` network radio.
//!
//! SoapySDR lets an application change settings at any time and opens each direction's
//! stream separately. A network radio takes its rates before the server session opens
//! and opens both directions together. This module bridges the two:
//!
//! 1. Settings are applied to an unsplit [`NetworkRadio`] as the application makes them,
//!    so coercion and refusals come back from the call that caused them.
//! 2. The first stream activation opens the server session for every stream set up so
//!    far. After that, rates are fixed and frequencies and RX gain retune live.
//! 3. Closing the last stream ends the session and reconnects with the recorded
//!    settings, so the application can change rates and stream again.

use std::time::{Duration, Instant};

use num_complex::Complex32;
use parking_lot::Mutex;
use sdr_hal::error::Error;
use sdr_hal::network::{AcceptedRanges, NetworkRadio, NetworkRadioOptions};
use sdr_hal::{DeviceTime, Radio, RadioTx, Schedule, TxCapability, TxMetadata, TxRadioHealth};

mod streams;
mod time;
use streams::{HeldSamples, RxHalf, TxHalf};
use time::{ns_to_ticks, ticks_to_ns, whole_hz};

/// Longest a read holds the RX stream, so a retune waits at most this long.
const READ_SLICE: Duration = Duration::from_millis(20);
/// How far ahead of device time untimed TX may run before a write waits. It bounds the
/// latency an untimed application sees and stays well inside the server's staging window.
const UNTIMED_LEAD: Duration = Duration::from_millis(100);
/// Longest an untimed write sleeps between device-time checks.
const UNTIMED_POLL: Duration = Duration::from_millis(5);
/// Interval between TX health reads while waiting for a status event.
const STATUS_POLL: Duration = Duration::from_millis(10);

/// Stream direction, numbered as SoapySDR numbers them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Samples from the application to the radio.
    Tx,
    /// Samples from the radio to the application.
    Rx,
}

impl Direction {
    fn label(self) -> &'static str {
        match self {
            Self::Tx => "TX",
            Self::Rx => "RX",
        }
    }
}

/// Result of one read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOutcome {
    /// `count` samples were written to the buffer, the first at `time_ns` device time.
    Samples {
        /// Valid samples at the start of the buffer.
        count: usize,
        /// Device time of the first sample in nanoseconds.
        time_ns: i64,
    },
    /// Samples were lost before the next sample. The next read returns that sample.
    Overflow,
    /// No samples arrived before the timeout.
    Timeout,
}

/// Result of one write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// Every sample was accepted.
    Written(usize),
    /// Untimed samples are already far enough ahead of device time that accepting more
    /// would have waited past the timeout. Nothing was sent.
    Timeout,
}

/// A TX event reported by the server's radio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxEvent {
    /// The device ran out of samples to send.
    Underflow,
    /// A timed block reached the device after its time.
    Late,
    /// Nothing happened before the timeout.
    Timeout,
}

/// When a block of TX samples is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxTiming {
    /// Directly after the previous untimed samples, or as soon as possible.
    Untimed,
    /// At this device time in nanoseconds.
    At(i64),
}

#[derive(Debug, Default, Clone)]
struct Settings {
    tick_rate: Option<u64>,
    tx_rate_hz: Option<f64>,
    tx_bandwidth_hz: Option<u64>,
    tx_frequency_hz: Option<u64>,
    rx_rate_hz: Option<f64>,
    rx_bandwidth_hz: Option<f64>,
    rx_frequency_hz: Option<f64>,
    rx_gain_db: Option<f64>,
    tx_gain_db: Option<f64>,
    rx_antenna: String,
}

enum Link {
    /// Settings are applied to this radio, and the first activation splits it.
    Configuring(Box<NetworkRadio>),
    /// The session is open and its halves are in the stream slots.
    Streaming,
    /// The previous connection ended and the next is made on demand.
    Disconnected,
}

#[derive(Debug, Default, Clone, Copy)]
struct StreamSet {
    tx: bool,
    rx: bool,
}

impl StreamSet {
    fn get(self, direction: Direction) -> bool {
        match direction {
            Direction::Tx => self.tx,
            Direction::Rx => self.rx,
        }
    }

    fn set(&mut self, direction: Direction, open: bool) {
        match direction {
            Direction::Tx => self.tx = open,
            Direction::Rx => self.rx = open,
        }
    }
}

struct Control {
    settings: Settings,
    link: Link,
    streams: StreamSet,
    tx_applied_hz: f64,
    rx_applied_hz: f64,
    tx_gain_db: Option<f64>,
    tick_rate: u64,
}

/// A network radio presented the way SoapySDR expects a device to behave.
///
/// Every method may be called from any thread. Reads and writes hold only their own
/// stream, so a blocking read does not stall a write or a retune for long.
pub struct Device {
    addr: String,
    options: NetworkRadioOptions,
    backend: String,
    supports_rx: bool,
    ranges: AcceptedRanges,
    tx_gain_range_db: Option<(f64, f64)>,
    control: Mutex<Control>,
    tx: Mutex<Option<TxHalf>>,
    rx: Mutex<Option<RxHalf>>,
}

impl Device {
    /// Connects to the control endpoint at `addr`, formatted `host:port`.
    pub fn open(addr: &str, options: NetworkRadioOptions) -> Result<Self, Error> {
        let radio = NetworkRadio::connect(addr, options.clone())?;
        Ok(Self {
            addr: addr.to_string(),
            backend: radio.backend().to_string(),
            supports_rx: radio.supports_rx(),
            ranges: radio.accepted_ranges(),
            tx_gain_range_db: radio.tx_gain_range_db(),
            control: Mutex::new(Control {
                settings: Settings::default(),
                tx_gain_db: radio.tx_gain_db(),
                tick_rate: radio.tick_rate(),
                link: Link::Configuring(Box::new(radio)),
                streams: StreamSet::default(),
                tx_applied_hz: 0.0,
                rx_applied_hz: 0.0,
            }),
            options,
            tx: Mutex::new(None),
            rx: Mutex::new(None),
        })
    }

    /// Backend name the server reported.
    pub fn backend(&self) -> &str {
        &self.backend
    }

    /// Control endpoint this device connects to.
    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// Whether the server can receive.
    pub fn supports_rx(&self) -> bool {
        self.supports_rx
    }

    /// Settings the server accepts. `None` ranges are unrestricted beyond the device.
    pub fn ranges(&self) -> &AcceptedRanges {
        &self.ranges
    }

    /// Device clock ticks per second.
    pub fn tick_rate(&self) -> u64 {
        self.control.lock().tick_rate
    }

    /// Complex samples per IQ packet this device requests.
    pub fn samples_per_packet(&self) -> usize {
        self.options
            .samples_per_packet
            .min(sdr_hal::network::wire::MAX_SAMPLES_PER_PACKET)
    }

    /// Sets the sample rate in Hz and returns the applied rate.
    pub fn set_sample_rate(&self, direction: Direction, rate_hz: f64) -> Result<f64, Error> {
        let mut control = self.control.lock();
        match direction {
            Direction::Tx => {
                positive(rate_hz, "TX sample rate")?;
                let rate = rate_hz;
                if control.is_streaming() {
                    return control
                        .unchanged(control.settings.tx_rate_hz == Some(rate), "TX sample rate")
                        .map(|()| control.tx_applied_hz);
                }
                let radio = self.radio(&mut control)?;
                radio.set_tx_sample_rate(rate)?;
                let applied = radio.tx_sample_rate_hz();
                let rx_applied = radio.rx_sample_rate_hz();
                control.settings.tx_rate_hz = Some(rate);
                control.tx_applied_hz = applied;
                if let Some(rx_applied) = rx_applied {
                    control.rx_applied_hz = rx_applied;
                }
                Ok(applied)
            }
            Direction::Rx => {
                positive(rate_hz, "RX sample rate")?;
                if control.is_streaming() {
                    return control
                        .unchanged(
                            control.settings.rx_rate_hz == Some(rate_hz),
                            "RX sample rate",
                        )
                        .map(|()| control.rx_applied_hz);
                }
                let mut settings = control.settings.clone();
                settings.rx_rate_hz = Some(rate_hz);
                self.apply_rx(&mut control, settings)?;
                Ok(control.rx_applied_hz)
            }
        }
    }

    /// Applied sample rate in Hz, or zero before one is set.
    pub fn sample_rate(&self, direction: Direction) -> f64 {
        let control = self.control.lock();
        match direction {
            Direction::Tx => control.tx_applied_hz,
            Direction::Rx => control.rx_applied_hz,
        }
    }

    /// Tunes the center frequency in Hz, live when the stream is open.
    pub fn set_frequency(&self, direction: Direction, frequency_hz: f64) -> Result<(), Error> {
        let mut control = self.control.lock();
        match direction {
            Direction::Tx => {
                let frequency = whole_hz(frequency_hz, "TX frequency")?;
                if control.is_streaming() {
                    let mut tx = self.tx.lock();
                    let half = tx.as_mut().ok_or_else(|| no_stream(Direction::Tx))?;
                    half.tx.set_tx_frequency_hz(frequency as f64)?;
                } else {
                    let frequency_usize = usize::try_from(frequency).map_err(|_| {
                        Error::from(format!("TX frequency {frequency} Hz exceeds usize"))
                    })?;
                    self.radio(&mut control)?
                        .set_tx_frequency(frequency_usize)?;
                }
                control.settings.tx_frequency_hz = Some(frequency);
            }
            Direction::Rx => {
                positive(frequency_hz, "RX frequency")?;
                if control.is_streaming()
                    && let Some(half) = self.rx.lock().as_mut()
                {
                    half.rx.set_rx_frequency(frequency_hz)?;
                    half.held = None;
                }
                control.settings.rx_frequency_hz = Some(frequency_hz);
            }
        }
        Ok(())
    }

    /// Requested center frequency in Hz, or zero before one is set.
    pub fn frequency(&self, direction: Direction) -> f64 {
        let control = self.control.lock();
        match direction {
            Direction::Tx => control.settings.tx_frequency_hz.unwrap_or(0) as f64,
            Direction::Rx => control.settings.rx_frequency_hz.unwrap_or(0.0),
        }
    }

    /// Sets the filter bandwidth in Hz. It cannot change while a session is open.
    pub fn set_bandwidth(&self, direction: Direction, bandwidth_hz: f64) -> Result<(), Error> {
        let mut control = self.control.lock();
        match direction {
            Direction::Tx => {
                let bandwidth = whole_hz(bandwidth_hz, "TX bandwidth")?;
                if control.is_streaming() {
                    return control.unchanged(
                        control.settings.tx_bandwidth_hz == Some(bandwidth),
                        "TX bandwidth",
                    );
                }
                let bandwidth_usize = usize::try_from(bandwidth).map_err(|_| {
                    Error::from(format!("TX bandwidth {bandwidth} Hz exceeds usize"))
                })?;
                self.radio(&mut control)?
                    .set_tx_bandwidth(bandwidth_usize)?;
                control.settings.tx_bandwidth_hz = Some(bandwidth);
                Ok(())
            }
            Direction::Rx => {
                positive(bandwidth_hz, "RX bandwidth")?;
                if control.is_streaming() {
                    return control.unchanged(
                        control.settings.rx_bandwidth_hz == Some(bandwidth_hz),
                        "RX bandwidth",
                    );
                }
                let mut settings = control.settings.clone();
                settings.rx_bandwidth_hz = Some(bandwidth_hz);
                self.apply_rx(&mut control, settings)
            }
        }
    }

    /// Requested filter bandwidth in Hz, or zero before one is set.
    pub fn bandwidth(&self, direction: Direction) -> f64 {
        let control = self.control.lock();
        match direction {
            Direction::Tx => control.settings.tx_bandwidth_hz.unwrap_or(0) as f64,
            Direction::Rx => control.settings.rx_bandwidth_hz.unwrap_or(0.0),
        }
    }

    /// Sets the RX gain in dB, live when the stream is open.
    pub fn set_rx_gain(&self, gain_db: f64) -> Result<(), Error> {
        if !gain_db.is_finite() {
            return Err(Error::from(format!("RX gain {gain_db} dB is not finite")));
        }
        let mut control = self.control.lock();
        if control.is_streaming() {
            if let Some(half) = self.rx.lock().as_mut() {
                half.rx.set_rx_gain(gain_db)?;
            }
            control.settings.rx_gain_db = Some(gain_db);
            return Ok(());
        }
        let mut settings = control.settings.clone();
        settings.rx_gain_db = Some(gain_db);
        self.apply_rx(&mut control, settings)
    }

    /// TX gain range in dB, or `None` when the server maintains its configured TX gain, as a
    /// composite server does.
    pub fn tx_gain_range(&self) -> Option<(f64, f64)> {
        self.tx_gain_range_db
    }

    /// Sets TX gain on the server and returns the applied gain, before or during streaming.
    pub fn set_tx_gain(&self, gain_db: f64) -> Result<f64, Error> {
        if !gain_db.is_finite() {
            return Err(Error::from(format!("TX gain {gain_db} dB is not finite")));
        }
        if self.tx_gain_range_db.is_none() {
            return Err("this server sets TX gain itself".into());
        }
        let mut control = self.control.lock();
        let applied = if control.is_streaming() {
            let mut tx = self.tx.lock();
            let half = tx.as_mut().ok_or_else(|| no_stream(Direction::Tx))?;
            half.tx.set_tx_gain(gain_db)?
        } else {
            self.radio(&mut control)?.set_tx_gain(gain_db)?
        };
        control.settings.tx_gain_db = Some(gain_db);
        control.tx_gain_db = Some(applied);
        Ok(applied)
    }

    /// Applied TX gain in dB, or `None` when the server maintains its configured TX gain.
    pub fn tx_gain(&self) -> Option<f64> {
        self.control.lock().tx_gain_db
    }

    /// Requested RX gain in dB, or zero before one is set.
    pub fn rx_gain(&self) -> f64 {
        self.control.lock().settings.rx_gain_db.unwrap_or(0.0)
    }

    /// Selects the RX port by its server name. Empty selects the server default.
    pub fn set_rx_antenna(&self, antenna: &str) -> Result<(), Error> {
        let mut control = self.control.lock();
        if control.is_streaming() {
            return control.unchanged(control.settings.rx_antenna == antenna, "RX antenna");
        }
        let mut settings = control.settings.clone();
        settings.rx_antenna = antenna.to_string();
        self.apply_rx(&mut control, settings)
    }

    /// Requested RX port, empty for the server default.
    pub fn rx_antenna(&self) -> String {
        self.control.lock().settings.rx_antenna.clone()
    }

    /// Requests the device clock rate and returns the applied rate. Sample rates already
    /// set are applied again, since the rates a device can reach follow its clock.
    pub fn set_tick_rate(&self, tick_rate: u64) -> Result<u64, Error> {
        let mut control = self.control.lock();
        if control.is_streaming() {
            return control
                .unchanged(control.tick_rate == tick_rate, "device clock rate")
                .map(|()| tick_rate);
        }
        let radio = self.radio(&mut control)?;
        let applied = radio.set_tick_rate(tick_rate)?;
        control.settings.tick_rate = Some(tick_rate);
        control.tick_rate = applied;
        let settings = control.settings.clone();
        self.reapply(&mut control, &settings)?;
        Ok(applied)
    }

    /// Declares that the application will stream in `direction`. Every stream must be
    /// set up before the first is activated, because activation opens the session.
    pub fn setup_stream(&self, direction: Direction) -> Result<(), Error> {
        if direction == Direction::Rx && !self.supports_rx {
            return Err("this server has no receiver".into());
        }
        let mut control = self.control.lock();
        if control.is_streaming() && !self.session_has(direction) {
            return Err(format!(
                "the open session has no {} stream. Close every stream, then set up all of them before activating one",
                direction.label()
            )
            .into());
        }
        control.streams.set(direction, true);
        Ok(())
    }

    /// Releases a stream. Releasing the last one ends the server session.
    pub fn close_stream(&self, direction: Direction) -> Result<(), Error> {
        let mut control = self.control.lock();
        control.streams.set(direction, false);
        if !control.is_streaming() || control.streams.tx || control.streams.rx {
            return Ok(());
        }
        let tx = self.tx.lock().take();
        let rx = self.rx.lock().take();
        if let Some(mut half) = rx
            && let Err(error) = half.rx.stop()
        {
            log::debug!("sdrhal: RX stop at stream close failed: {error}");
        }
        if let Some(mut half) = tx
            && half.rate_hz > 0.0
            && let Err(error) = half.tx.enable_transmit(false)
        {
            log::debug!("sdrhal: TX stop at stream close failed: {error}");
        }
        control.link = Link::Disconnected;
        // A failed reconnect stays disconnected until the next call needs the radio.
        if let Err(error) = self.radio(&mut control) {
            log::warn!("sdrhal: reconnect to {} failed: {error}", self.addr);
        }
        Ok(())
    }

    /// Starts a stream, opening the server session on the first activation.
    /// `time_ns` schedules the start against device time.
    pub fn activate(&self, direction: Direction, time_ns: Option<i64>) -> Result<(), Error> {
        let mut control = self.control.lock();
        if !control.streams.get(direction) {
            return Err(format!(
                "set up the {} stream before activating it",
                direction.label()
            )
            .into());
        }
        if !control.is_streaming() {
            self.open_session(&mut control)?;
        }
        let tick_rate = control.tick_rate;
        let ticks = time_ns.map(|ns| ns_to_ticks(ns, tick_rate)).transpose()?;
        match direction {
            Direction::Tx => {
                let mut tx = self.tx.lock();
                let half = tx.as_mut().ok_or_else(|| no_stream(Direction::Tx))?;
                half.tx.enable_transmit_at(true, ticks)?;
                half.pacing = None;
            }
            Direction::Rx => {
                let mut rx = self.rx.lock();
                let half = rx.as_mut().ok_or_else(|| no_stream(Direction::Rx))?;
                half.rx.rx_activate(ticks)?;
                half.held = None;
            }
        }
        Ok(())
    }

    /// Stops a stream without ending the session.
    pub fn deactivate(&self, direction: Direction) -> Result<(), Error> {
        match direction {
            Direction::Tx => {
                if let Some(half) = self.tx.lock().as_mut() {
                    half.tx.enable_transmit(false)?;
                    half.pacing = None;
                }
            }
            Direction::Rx => {
                if let Some(half) = self.rx.lock().as_mut() {
                    half.rx.stop()?;
                    half.held = None;
                }
            }
        }
        Ok(())
    }

    /// Reads up to `buf.len()` samples, waiting at most `timeout`.
    ///
    /// A gap is reported as [`ReadOutcome::Overflow`] before the samples that follow it,
    /// which the next read returns.
    pub fn read(&self, buf: &mut [Complex32], timeout: Duration) -> Result<ReadOutcome, Error> {
        let deadline = Instant::now() + timeout;
        loop {
            let mut rx = self.rx.lock();
            let half = rx.as_mut().ok_or_else(|| no_stream(Direction::Rx))?;
            if let Some(outcome) = half.take_held(buf) {
                return Ok(outcome);
            }
            let slice = deadline
                .saturating_duration_since(Instant::now())
                .min(READ_SLICE);
            let result = half.rx.rx_read(buf, slice.as_micros() as i64)?;
            if result.samples_read > 0 {
                if result.overflow {
                    half.held = Some(HeldSamples {
                        samples: buf[..result.samples_read].to_vec(),
                        time_ticks: result.time_ticks,
                    });
                    return Ok(ReadOutcome::Overflow);
                }
                return Ok(ReadOutcome::Samples {
                    count: result.samples_read,
                    time_ns: ticks_to_ns(result.time_ticks, half.tick_rate),
                });
            }
            if result.overflow {
                return Ok(ReadOutcome::Overflow);
            }
            drop(rx);
            if Instant::now() >= deadline {
                return Ok(ReadOutcome::Timeout);
            }
        }
    }

    /// Sends samples, ending the burst after them when `end_of_burst` is set.
    ///
    /// Untimed samples are kept at most [`UNTIMED_LEAD`] ahead of device time, so a write
    /// waits rather than overrunning the server. A timed write never waits.
    pub fn write(
        &self,
        samples: &[Complex32],
        timing: TxTiming,
        end_of_burst: bool,
        timeout: Duration,
    ) -> Result<WriteOutcome, Error> {
        let deadline = Instant::now() + timeout;
        loop {
            let mut tx = self.tx.lock();
            let half = tx.as_mut().ok_or_else(|| no_stream(Direction::Tx))?;
            if half.rate_hz <= 0.0 {
                return Err(no_stream(Direction::Tx));
            }
            let schedule = match timing {
                TxTiming::At(time_ns) => Schedule::At(DeviceTime::from_ticks(ns_to_ticks(
                    time_ns,
                    half.tick_rate,
                )?)),
                TxTiming::Untimed => {
                    let lead_ticks = half.untimed_lead_ticks(samples.len())?;
                    let limit_ticks = (UNTIMED_LEAD.as_nanos() * half.tick_rate as u128
                        / Duration::from_secs(1).as_nanos())
                        as u64;
                    if lead_ticks > limit_ticks {
                        drop(tx);
                        let now = Instant::now();
                        if now >= deadline {
                            return Ok(WriteOutcome::Timeout);
                        }
                        std::thread::sleep(UNTIMED_POLL.min(deadline - now));
                        continue;
                    }
                    Schedule::Immediate
                }
            };
            // A server that cannot mark a burst end still sends every sample, so the
            // marker is dropped rather than the samples.
            let end_of_burst =
                end_of_burst && half.tx.capabilities().supports(TxCapability::BurstEnd);
            half.tx.write_all(
                samples,
                TxMetadata {
                    schedule,
                    end_of_burst,
                },
            )?;
            if let (TxTiming::Untimed, Some(pacing)) = (timing, half.pacing.as_mut()) {
                pacing.sent_samples += samples.len() as u64;
            }
            return Ok(WriteOutcome::Written(samples.len()));
        }
    }

    /// Waits up to `timeout` for the next TX underflow or late block. Before TX is active
    /// there is nothing to report, so the wait ends in a timeout.
    pub fn tx_event(&self, timeout: Duration) -> Result<TxEvent, Error> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(half) = self.tx.lock().as_mut() {
                let now = half.tx.tx_health()?;
                let last = std::mem::replace(&mut half.health, now);
                if now.late_packets > last.late_packets {
                    return Ok(TxEvent::Late);
                }
                if now.underflows > last.underflows {
                    return Ok(TxEvent::Underflow);
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(TxEvent::Timeout);
            }
            std::thread::sleep(STATUS_POLL.min(deadline - now));
        }
    }

    /// Device time in nanoseconds.
    pub fn hardware_time_ns(&self) -> Result<i64, Error> {
        if let Some(half) = self.tx.lock().as_ref() {
            return Ok(ticks_to_ns(half.tx.get_hardware_time()?, half.tick_rate));
        }
        let mut control = self.control.lock();
        let tick_rate = control.tick_rate;
        let ticks = self.radio(&mut control)?.hardware_time()?;
        Ok(ticks_to_ns(ticks, tick_rate))
    }

    /// Resets device time. It needs an open session, and a composite server refuses it.
    pub fn set_hardware_time_ns(&self, time_ns: i64) -> Result<(), Error> {
        let tx = self.tx.lock();
        let half = tx
            .as_ref()
            .ok_or_else(|| Error::from("activate a stream before setting device time"))?;
        half.tx
            .set_hardware_time(ns_to_ticks(time_ns, half.tick_rate)?)
    }

    /// The unsplit radio, reconnecting with the recorded settings if the last session
    /// ended. Refused while a session is open.
    fn radio<'a>(&self, control: &'a mut Control) -> Result<&'a mut NetworkRadio, Error> {
        if matches!(control.link, Link::Disconnected) {
            let radio = NetworkRadio::connect(&self.addr, self.options.clone())?;
            control.tick_rate = radio.tick_rate();
            control.link = Link::Configuring(Box::new(radio));
            let settings = control.settings.clone();
            self.reapply(control, &settings)?;
        }
        match &mut control.link {
            Link::Configuring(radio) => Ok(radio),
            Link::Streaming => Err("a session is open, so close every stream first".into()),
            Link::Disconnected => unreachable!("reconnected above"),
        }
    }

    /// Applies every recorded setting to a fresh or reclocked radio.
    fn reapply(&self, control: &mut Control, settings: &Settings) -> Result<(), Error> {
        let Link::Configuring(radio) = &mut control.link else {
            return Err("a session is open, so close every stream first".into());
        };
        if let Some(tick_rate) = settings.tick_rate
            && radio.tick_rate() != tick_rate
        {
            control.tick_rate = radio.set_tick_rate(tick_rate)?;
        }
        if let Some(rate) = settings.tx_rate_hz {
            radio.set_tx_sample_rate(rate)?;
            control.tx_applied_hz = radio.tx_sample_rate_hz();
        }
        if let Some(bandwidth) = settings.tx_bandwidth_hz {
            radio.set_tx_bandwidth(bandwidth as usize)?;
        }
        if let Some(frequency) = settings.tx_frequency_hz {
            radio.set_tx_frequency(frequency as usize)?;
        }
        if let Some(gain_db) = settings.tx_gain_db {
            control.tx_gain_db = Some(radio.set_tx_gain(gain_db)?);
        }
        self.apply_rx(control, settings.clone())
    }

    /// Configures RX from `settings` once it names a rate, then records them.
    fn apply_rx(&self, control: &mut Control, settings: Settings) -> Result<(), Error> {
        if let Some(rate_hz) = settings.rx_rate_hz {
            let radio = self.radio(control)?;
            let applied = radio.setup_rx(
                0,
                &settings.rx_antenna,
                rate_hz,
                settings.rx_bandwidth_hz.unwrap_or(rate_hz),
                settings.rx_gain_db,
            )?;
            let tx_applied = radio.tx_sample_rate_hz();
            control.rx_applied_hz = applied;
            if tx_applied > 0.0 {
                control.tx_applied_hz = tx_applied;
            }
        }
        control.settings = settings;
        Ok(())
    }

    fn open_session(&self, control: &mut Control) -> Result<(), Error> {
        if control.streams.tx && control.settings.tx_rate_hz.is_none() {
            return Err("set the TX sample rate before activating the TX stream".into());
        }
        if control.streams.rx && control.settings.rx_rate_hz.is_none() {
            return Err("set the RX sample rate before activating the RX stream".into());
        }
        self.radio(control)?;
        let Link::Configuring(radio) = std::mem::replace(&mut control.link, Link::Disconnected)
        else {
            unreachable!("radio() leaves the link configuring");
        };
        let (tx, rx) = (radio as Box<dyn Radio>).split()?;
        let tick_rate = tx.tick_rate();
        let rx = rx
            .map(|mut rx| -> Result<RxHalf, Error> {
                if let Some(frequency_hz) = control.settings.rx_frequency_hz {
                    rx.set_rx_frequency(frequency_hz)?;
                }
                Ok(RxHalf {
                    rx,
                    tick_rate,
                    held: None,
                })
            })
            .transpose()?;
        *self.tx.lock() = Some(TxHalf {
            rate_hz: tx.tx_sample_rate_hz(),
            tick_rate,
            tx,
            pacing: None,
            health: TxRadioHealth::default(),
        });
        *self.rx.lock() = rx;
        control.tick_rate = tick_rate;
        control.link = Link::Streaming;
        Ok(())
    }

    fn session_has(&self, direction: Direction) -> bool {
        match direction {
            Direction::Tx => self
                .tx
                .lock()
                .as_ref()
                .is_some_and(|half| half.rate_hz > 0.0),
            Direction::Rx => self.rx.lock().is_some(),
        }
    }
}

impl Control {
    fn is_streaming(&self) -> bool {
        matches!(self.link, Link::Streaming)
    }

    /// Accepts a setting that matches the open session and refuses a change to it.
    fn unchanged(&self, matches: bool, what: &str) -> Result<(), Error> {
        if matches {
            Ok(())
        } else {
            Err(format!(
                "the {what} cannot change while a session is open. Close every stream first"
            )
            .into())
        }
    }
}

fn positive(value: f64, what: &str) -> Result<(), Error> {
    if value.is_finite() && value > 0.0 {
        Ok(())
    } else {
        Err(format!("{what} {value} Hz must be finite and positive").into())
    }
}

fn no_stream(direction: Direction) -> Error {
    format!("no {} stream is active on this device", direction.label()).into()
}
