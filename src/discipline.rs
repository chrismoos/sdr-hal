//! A device clock disciplined to an external reference pulse.
//!
//! The device oscillator is left free-running. A map from device ticks to disciplined
//! ticks follows the reference, and the stream halves resample and frequency-shift
//! their samples through it, so clients see a clock and a carrier that track the
//! reference.

mod interp;
mod map;
mod rx;
mod servo;
mod timestamps;
mod tx;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use num_complex::Complex32;

use crate::dsp::PhasorNco;
use crate::error::Error;
use crate::{RadioRx, RadioTx};
use map::{Position, SharedMap};
use servo::{Servo, SharedStatus};

/// How often the runner reads the device while it does not know when the next pulse is due.
const NOMINAL_RATE_TOLERANCE: f64 = 1e-7;
const POLL: Duration = Duration::from_millis(250);
/// How long after a pulse is due the runner reads it. The device has latched the pulse
/// and the receiver has reported its second by then.
const READ_AFTER_PULSE: Duration = Duration::from_millis(150);
/// The runner reads the device at least this often, so a stop request is seen promptly.
const LONGEST_WAIT: Duration = Duration::from_millis(1100);
const PULSE_PERIOD: Duration = Duration::from_secs(1);
/// The oldest a receiver report can be when it is read, in seconds. A receiver that
/// reports twice a second has its report replaced within this time.
const REPORT_MAX_AGE_S: f64 = 0.6;
/// Slack on either side of the report age when matching a report to a pulse.
const REPORT_AGE_SLACK_S: f64 = 0.05;

/// The most recent report of a reference receiver.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReceiverReport {
    /// The time the report states, in Unix seconds. It is the time the report was
    /// measured at, not the time it was read. `None` before the receiver knows the time.
    pub unix_seconds: Option<f64>,
    /// Whether the receiver's pulses are locked to its reference.
    pub locked: bool,
    /// Satellites the receiver uses, when it reports the count.
    pub satellites: Option<u8>,
    /// Where the receiver is, when it has a fix.
    pub position: Option<crate::ReceiverPosition>,
    /// Horizontal dilution of precision of the fix.
    pub hdop: Option<f64>,
}

/// A reference pulse wired to a device, and the receiver that produces it.
pub trait PulseSource: Send + 'static {
    /// The device clock now, in ticks.
    fn device_ticks(&mut self) -> Result<u64, Error>;
    /// The device clock latched at the most recent pulse, in ticks.
    fn last_pulse_ticks(&mut self) -> Result<u64, Error>;
    /// The receiver's most recent report. It must not wait for a new one.
    fn receiver_report(&mut self) -> Result<ReceiverReport, Error>;
}

/// The Unix second of the pulse `since_pulse_s` seconds before a report was read, or
/// `None` when the report is too old or too new to belong to that pulse.
fn pulse_second(report_unix_seconds: f64, since_pulse_s: f64) -> Option<u64> {
    // The pulse second lies between the report time less the time since the pulse,
    // and that plus the report's greatest age.
    let middle = report_unix_seconds - since_pulse_s + REPORT_MAX_AGE_S / 2.0;
    let second = middle.round();
    ((middle - second).abs() <= REPORT_MAX_AGE_S / 2.0 + REPORT_AGE_SLACK_S && second >= 0.0)
        .then_some(second as u64)
}

pub(crate) struct Shared {
    pub(crate) map: Arc<SharedMap>,
    pub(crate) status: Arc<SharedStatus>,
    stop: Arc<AtomicBool>,
    runner: std::sync::Mutex<Option<JoinHandle<()>>>,
}

impl Shared {
    pub(crate) fn disciplined_ticks(&self, device_ticks: u64) -> Result<u64, Error> {
        Ok(self
            .map
            .snapshot()
            .disciplined_at(Position::from_ticks(device_ticks))
            .round())
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(runner) = self.runner.lock().ok().and_then(|mut runner| runner.take()) {
            let _ = runner.join();
        }
    }
}

/// Rotates samples spaced `ticks_per_sample` apart by `carrier_hz` times the time the
/// device clock has gained on the disciplined clock. The first sample is where the
/// device clock is `ahead_ticks` ahead, and the gap grows at `rate_error`.
/// The sample rate a client of a disciplined stream works at. A device often applies a
/// rate a few parts per billion from the requested one, and the stream is resampled
/// to the whole-hertz rate when the applied rate is within `NOMINAL_RATE_TOLERANCE`
/// of it.
fn client_rate_hz(applied_hz: f64) -> f64 {
    let whole = applied_hz.round();
    if (applied_hz - whole).abs() <= applied_hz * NOMINAL_RATE_TOLERANCE {
        whole
    } else {
        applied_hz
    }
}

fn rotate(
    samples: &mut [Complex32],
    carrier_hz: f64,
    ahead_ticks: f64,
    rate_error: f64,
    ticks_per_sample: f64,
    tick_rate: f64,
) {
    let cycles = carrier_hz * ahead_ticks / tick_rate;
    let phase = std::f64::consts::TAU * cycles.rem_euclid(1.0);
    let step = std::f64::consts::TAU * carrier_hz * ticks_per_sample * rate_error
        / (1.0 + rate_error)
        / tick_rate;
    PhasorNco::with_start_phase(phase, step).rotate_in_place(samples);
}

fn run(mut source: Box<dyn PulseSource>, mut servo: Servo, tick_rate: u64, stop: &AtomicBool) {
    let mut last_pulse = None;
    let mut failures = 0u64;
    while !stop.load(Ordering::Acquire) {
        // Every read here shares the device control channel with the streams, so the
        // runner reads once per pulse where it can.
        let step = (|| -> Result<Duration, Error> {
            let pulse = source.last_pulse_ticks()?;
            let report = source.receiver_report()?;
            let now = source.device_ticks()?;
            // A pulse between the reads would pair the report with the wrong second.
            if source.last_pulse_ticks()? != pulse {
                return Ok(READ_AFTER_PULSE);
            }
            servo.set_receiver(&report);
            let since_pulse =
                Duration::from_secs_f64(now.saturating_sub(pulse) as f64 / tick_rate as f64);
            let is_new = last_pulse != Some(pulse);
            if is_new {
                // The first read is the pulse before the runner started, of unknown age.
                if last_pulse.is_some() {
                    servo.observe(pulse, now);
                    if let Some(second) = report
                        .unix_seconds
                        .and_then(|seconds| pulse_second(seconds, since_pulse.as_secs_f64()))
                    {
                        servo.label(pulse, second);
                    }
                }
                last_pulse = Some(pulse);
            }
            servo.poll(now);
            let next_read = PULSE_PERIOD + READ_AFTER_PULSE;
            Ok(if since_pulse < next_read {
                next_read - since_pulse
            } else {
                POLL
            })
        })();
        let wait = match step {
            Ok(wait) => {
                failures = 0;
                wait
            }
            Err(error) => {
                failures += 1;
                if failures.is_power_of_two() {
                    log::warn!("clock discipline: reading the reference failed: {error}");
                }
                POLL
            }
        };
        std::thread::sleep(wait.min(LONGEST_WAIT));
    }
}

/// Wraps the halves of a radio so their timestamps count ticks of a clock disciplined
/// to `source`, and their carriers follow it. `tx_carrier_hz` and `rx_carrier_hz` are
/// the frequencies the halves are tuned to, when they are tuned.
pub fn discipline(
    tx: Box<dyn RadioTx>,
    rx: Option<Box<dyn RadioRx>>,
    source: Box<dyn PulseSource>,
    tx_carrier_hz: Option<f64>,
    rx_carrier_hz: Option<f64>,
    gps: crate::Gps,
) -> Result<(Box<dyn RadioTx>, Option<Box<dyn RadioRx>>), Error> {
    let tick_rate = tx.tick_rate();
    if tick_rate == 0 {
        return Err(Error::invalid_setting(
            "a disciplined clock needs a device tick rate",
        ));
    }
    let map = SharedMap::new();
    let status = SharedStatus::new();
    let correct = gps == crate::Gps::Pps;
    let servo = Servo::new(tick_rate, Arc::clone(&map), Arc::clone(&status), correct);
    let stop = Arc::new(AtomicBool::new(false));
    let runner = std::thread::Builder::new()
        .name("clock-discipline".to_string())
        .spawn({
            let stop = Arc::clone(&stop);
            move || run(source, servo, tick_rate, &stop)
        })
        .map_err(|error| Error::invalid_setting(format!("clock discipline thread: {error}")))?;
    let shared = Arc::new(Shared {
        map,
        status,
        stop,
        runner: std::sync::Mutex::new(Some(runner)),
    });
    if !correct {
        let rx = rx.map(|inner| -> Box<dyn RadioRx> {
            Box::new(timestamps::TimestampedRx {
                inner,
                _shared: Arc::clone(&shared),
            })
        });
        return Ok((
            Box::new(timestamps::TimestampedTx { inner: tx, shared }),
            rx,
        ));
    }
    let kernel = Arc::new(interp::Kernel::new());
    let tx: Box<dyn RadioTx> = Box::new(tx::DisciplinedTx::new(
        tx,
        Arc::clone(&shared),
        Arc::clone(&kernel),
        tx_carrier_hz,
    ));
    let rx = rx.map(|rx| -> Box<dyn RadioRx> {
        Box::new(rx::DisciplinedRx::new(rx, shared, kernel, rx_carrier_hz))
    });
    Ok((tx, rx))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod pulse_second_tests {
    use super::pulse_second;

    const SECOND: f64 = 1_795_602_425.0;

    #[test]
    fn a_report_read_at_any_point_in_the_second_belongs_to_the_last_pulse() {
        // A receiver reporting twice a second: the report for the pulse itself, then
        // the one half a second later, each read at several ages.
        for (report, since_pulse) in [
            (SECOND, 0.09),
            (SECOND, 0.43),
            (SECOND, 0.53),
            (SECOND + 0.5, 0.64),
            (SECOND + 0.5, 0.99),
            (SECOND - 0.5, 0.03),
        ] {
            assert_eq!(
                pulse_second(report, since_pulse),
                Some(SECOND as u64),
                "report={report} since_pulse={since_pulse}"
            );
        }
    }

    #[test]
    fn a_report_that_fits_no_pulse_is_not_used() {
        // A report this old has been replaced, and a newer one cannot exist yet.
        assert_eq!(pulse_second(SECOND, 0.8), None);
        assert_eq!(pulse_second(SECOND + 0.5, 0.3), None);
    }
}
