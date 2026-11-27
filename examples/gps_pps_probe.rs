//! Opens a USRP with GPS time and prints the state of its disciplined clock.
//!
//! `gps_pps_probe [device args] [seconds] [outage start] [outage seconds]` withholds
//! the receiver's pulses for the outage, so the clock runs in holdover on the real
//! oscillator. The pulse offset printed when pulses return is the holdover drift.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sdr_hal::discipline::{PulseSource, ReceiverReport};
use sdr_hal::error::Error;
use sdr_hal::{Radio, UhdRadio};

/// Reports no new pulse and no fix while the outage lasts.
struct Outage {
    inner: Box<dyn PulseSource>,
    started: Instant,
    from: Duration,
    until: Duration,
    held_pulse: u64,
}

impl Outage {
    fn active(&self) -> bool {
        let elapsed = self.started.elapsed();
        elapsed >= self.from && elapsed < self.until
    }
}

impl PulseSource for Outage {
    fn device_ticks(&mut self) -> Result<u64, Error> {
        self.inner.device_ticks()
    }

    fn last_pulse_ticks(&mut self) -> Result<u64, Error> {
        if !self.active() {
            self.held_pulse = self.inner.last_pulse_ticks()?;
        }
        Ok(self.held_pulse)
    }

    fn receiver_report(&mut self) -> Result<ReceiverReport, Error> {
        let mut report = self.inner.receiver_report()?;
        report.locked &= !self.active();
        Ok(report)
    }
}

const TX_SAMPLE_RATE_HZ: usize = 4_915_200;
const TX_FREQUENCY_HZ: usize = 890_730_000;
const TX_GAIN_DB: f64 = 0.0;
const REPORT: Duration = Duration::from_secs(1);
/// An outage start no run reaches. Half the range leaves room to add the outage length.
const NO_OUTAGE: u64 = u64::MAX / 2;
const NANOS_PER_SECOND: f64 = 1e9;
/// 1980-01-06T00:00:00Z as Unix seconds.
const REFERENCE_EPOCH_UNIX_SECONDS: u64 = 315_964_800;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let mut args = std::env::args().skip(1);
    let device_args = args.next().unwrap_or_else(|| "type=b200".to_string());
    let seconds: u64 = args.next().map_or(Ok(60), |value| value.parse())?;

    let open = || {
        UhdRadio::new(
            &device_args,
            0,
            "TX/RX",
            TX_GAIN_DB,
            None,
            None,
            Some(TX_SAMPLE_RATE_HZ),
        )
    };
    let mut radio = open()?;
    if !radio.has_gps() {
        drop(radio);
        radio = open()?;
    }
    let outage_from: u64 = args.next().map_or(Ok(NO_OUTAGE), |value| value.parse())?;
    let outage_seconds: u64 = args.next().map_or(Ok(0), |value| value.parse())?;
    let started = Instant::now();
    let mut radio = radio.with_gps_through(sdr_hal::Gps::Pps, move |inner| {
        Box::new(Outage {
            inner,
            started,
            from: Duration::from_secs(outage_from),
            until: Duration::from_secs(outage_from + outage_seconds),
            held_pulse: 0,
        })
    })?;
    radio.set_tx_frequency(TX_FREQUENCY_HZ)?;
    let tick_rate = radio.tick_rate();
    let (tx, _rx) = Box::new(radio).split()?;
    println!("tick rate {tick_rate}");

    while started.elapsed() < Duration::from_secs(seconds) {
        std::thread::sleep(REPORT);
        let status = tx
            .clock_status()
            .expect("a disciplined clock reports its status");
        let reference = tx
            .time_reference()?
            .expect("a disciplined clock reports a reference");
        let now_ticks = tx.get_hardware_time()?;
        let host = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64()
            - REFERENCE_EPOCH_UNIX_SECONDS as f64;
        let clock = reference.seconds as f64
            + f64::from(reference.subsecond_ns) / NANOS_PER_SECOND
            + (now_ticks as f64 - reference.device_ticks as f64) / tick_rate as f64;
        println!(
            "t={:5.1}s lock={:?} fix={} satellites={:?} stability={:?} ppb rate={:+9.2} ppb phase={:+7.1} ns accepted={} rejected={} uncertainty={} ns clock-host={:+.3} s",
            started.elapsed().as_secs_f64(),
            status.lock,
            status.receiver_fix,
            status.satellites,
            status.rate_stability_ppb,
            status.rate_error_ppb,
            status.phase_error_ns,
            status.accepted_pulses,
            status.rejected_pulses,
            reference.uncertainty_ns,
            if reference.seconds == 0 {
                f64::NAN
            } else {
                clock - host
            },
        );
    }
    Ok(())
}
