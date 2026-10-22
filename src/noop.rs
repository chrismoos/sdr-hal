use crate::{Radio, RadioRx, RadioTx, RxReadResult, error::Error};
use num_complex::Complex32;
use std::{
    thread,
    time::{Duration, Instant},
};
/// Paced immediate TX sink and optional silent RX source, with nanosecond timestamps.
/// Scheduled writes are accepted before their timestamp.
///
/// TX time follows wall time since construction. RX time starts from TX time at
/// activation, then follows the sample count. Timed RX activation is unsupported.
pub struct NoopRadio {
    tx_sample_rate: f64,
    tx_enabled: bool,
    next_tx_deadline: Option<Instant>,
    clock_start: Instant,
    rx_sample_rate: Option<usize>,
}

impl NoopRadio {
    /// Creates a TX sink at `tx_sample_rate_hz`. A zero rate disables TX pacing.
    /// Call [`Radio::setup_rx`] before splitting to obtain silent RX.
    pub fn new(tx_sample_rate_hz: usize) -> NoopRadio {
        NoopRadio {
            tx_sample_rate: tx_sample_rate_hz as f64,
            tx_enabled: false,
            next_tx_deadline: None,
            clock_start: Instant::now(),
            rx_sample_rate: None,
        }
    }
}

impl Radio for NoopRadio {
    fn tx_capabilities(&self) -> crate::TxCapabilities {
        let capabilities = crate::TxCapabilities::from([crate::TxCapability::BurstEnd]);
        if self.supports_timed_tx() {
            capabilities.with(crate::TxCapability::TimedSamples)
        } else {
            capabilities
        }
    }

    fn tx_sample_rate_hz(&self) -> f64 {
        self.tx_sample_rate
    }
    fn rx_sample_rate_hz(&self) -> Option<f64> {
        self.rx_sample_rate.map(|rate| rate as f64)
    }
    fn supports_timed_tx(&self) -> bool {
        true
    }
    fn tick_rate(&self) -> u64 {
        1_000_000_000
    }

    fn set_tx_frequency(&mut self, _center_frequency: usize) -> Result<(), Error> {
        Ok(())
    }

    fn hardware_time(&self) -> Result<u64, Error> {
        Ok(self.clock_start.elapsed().as_nanos() as u64)
    }

    fn set_tx_sample_rate(&mut self, sample_rate_hz: f64) -> Result<f64, Error> {
        crate::stream::positive_hz(sample_rate_hz, "set_tx_sample_rate")?;
        self.tx_sample_rate = sample_rate_hz;
        Ok(sample_rate_hz)
    }

    fn set_tx_bandwidth(&mut self, _bandwidth: usize) -> Result<(), Error> {
        Ok(())
    }

    fn setup_rx(
        &mut self,
        _channel: usize,
        _antenna: &str,
        sample_rate_hz: f64,
        _bandwidth_hz: f64,
        _gain_db: Option<f64>,
    ) -> Result<f64, Error> {
        if !sample_rate_hz.is_finite() || sample_rate_hz <= 0.0 {
            return Err("RX sample rate must be finite and positive".into());
        }
        self.rx_sample_rate = Some(sample_rate_hz.round() as usize);
        Ok(self.rx_sample_rate_hz().expect("RX rate was set"))
    }

    fn split(self: Box<Self>) -> Result<(Box<dyn RadioTx>, Option<Box<dyn RadioRx>>), Error> {
        let tx = NoopTxHalf {
            tx_sample_rate: self.tx_sample_rate,
            tx_enabled: self.tx_enabled,
            next_tx_deadline: self.next_tx_deadline,
            clock_start: self.clock_start,
        };
        let rx = self.rx_sample_rate.map(|rate| {
            Box::new(NoopRxHalf {
                sample_rate: rate,
                next_rx_deadline: None,
                clock_start: self.clock_start,
                activation_ticks: 0,
                samples_emitted: 0,
            }) as Box<dyn RadioRx>
        });
        Ok((Box::new(tx), rx))
    }
}

struct NoopTxHalf {
    tx_sample_rate: f64,
    tx_enabled: bool,
    next_tx_deadline: Option<Instant>,
    clock_start: Instant,
}

impl NoopTxHalf {
    fn simulate_tx_timing_samples(&mut self, sample_count: usize) {
        if !self.tx_enabled || sample_count == 0 || self.tx_sample_rate <= 0.0 {
            return;
        }

        let tx_duration = Duration::from_secs_f64(sample_count as f64 / self.tx_sample_rate);

        let deadline = self.next_tx_deadline.unwrap_or_else(Instant::now);
        let now = Instant::now();
        if deadline > now {
            thread::sleep(deadline.duration_since(now));
        }
        self.next_tx_deadline = Some(deadline + tx_duration);
    }
}

impl RadioTx for NoopTxHalf {
    fn write(
        &mut self,
        samples: &[Complex32],
        metadata: crate::TxMetadata,
    ) -> Result<usize, Error> {
        match metadata.schedule {
            crate::Schedule::Immediate => self.transmit(samples)?,
            crate::Schedule::At(time) => {
                let deadline = self
                    .clock_start
                    .checked_add(Duration::from_nanos(time.ticks()))
                    .ok_or_else(|| {
                        Error::invalid_setting("TX timestamp exceeds the clock range")
                    })?;
                if self.tx_enabled && deadline <= Instant::now() {
                    return Err(Error::new(
                        crate::error::ErrorKind::Late,
                        "scheduled no-op TX timestamp has passed",
                    )
                    .with_accepted_samples(0));
                }
                let duration = Duration::from_secs_f64(
                    samples.len() as f64 / self.tx_sample_rate.max(f64::MIN_POSITIVE),
                );
                self.next_tx_deadline = Some(deadline.checked_add(duration).ok_or_else(|| {
                    Error::invalid_setting("TX duration exceeds the clock range")
                })?);
            }
        }
        if metadata.end_of_burst {
            self.end_burst()?;
        }
        Ok(samples.len())
    }

    fn capabilities(&self) -> crate::TxCapabilities {
        crate::TxCapabilities::from([
            crate::TxCapability::TimedSamples,
            crate::TxCapability::BurstEnd,
        ])
    }
    fn end_burst(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn tx_sample_rate_hz(&self) -> f64 {
        self.tx_sample_rate
    }
    fn set_tx_frequency_hz(&mut self, _frequency_hz: f64) -> Result<(), Error> {
        Ok(())
    }

    fn supports_timed_tx(&self) -> bool {
        true
    }
    fn tick_rate(&self) -> u64 {
        1_000_000_000
    }

    fn get_hardware_time(&self) -> Result<u64, Error> {
        Ok(self.clock_start.elapsed().as_nanos() as u64)
    }

    fn transmit(&mut self, samples: &[Complex32]) -> Result<(), Error> {
        self.simulate_tx_timing_samples(samples.len());
        Ok(())
    }

    fn transmit_at(&mut self, samples: &[Complex32], tick: Option<u64>) -> Result<(), Error> {
        if let Some(tick) = tick {
            self.next_tx_deadline = Some(self.clock_start + Duration::from_nanos(tick));
        }
        self.simulate_tx_timing_samples(samples.len());
        Ok(())
    }

    fn enable_transmit(&mut self, enable: bool) -> Result<(), Error> {
        self.tx_enabled = enable;
        self.next_tx_deadline = enable.then(Instant::now);
        Ok(())
    }

    fn enable_transmit_at(&mut self, enable: bool, _tick: Option<u64>) -> Result<(), Error> {
        self.tx_enabled = enable;
        self.next_tx_deadline = enable.then(Instant::now);
        Ok(())
    }
}

struct NoopRxHalf {
    sample_rate: usize,
    next_rx_deadline: Option<Instant>,
    clock_start: Instant,
    activation_ticks: u64,
    samples_emitted: u64,
}

impl NoopRxHalf {
    /// Hardware-time stamp (ns) for the next sample to be delivered. Uses ceil
    /// division so the pipeline's `time_ticks -> absolute_sample` floor maps it
    /// back to exactly `samples_emitted` (tick rate is 1 GHz. sample rate is
    /// well under 1 GHz), keeping the stream contiguous.
    #[inline]
    fn contiguous_time_ticks(&self) -> u64 {
        let sr = self.sample_rate.max(1) as u128;
        self.activation_ticks
            + (self.samples_emitted as u128 * 1_000_000_000u128).div_ceil(sr) as u64
    }
}

impl RadioRx for NoopRxHalf {
    fn rx_sample_rate_hz(&self) -> f64 {
        self.sample_rate as f64
    }
    fn set_rx_frequency(&mut self, _frequency_hz: f64) -> Result<(), Error> {
        Ok(())
    }

    fn tick_rate(&self) -> u64 {
        1_000_000_000
    }

    fn get_hardware_time(&self) -> Result<u64, Error> {
        Ok(self.contiguous_time_ticks())
    }

    fn rx_read(&mut self, buf: &mut [Complex32], timeout_us: i64) -> Result<RxReadResult, Error> {
        let batch_ns =
            (buf.len() as u128).saturating_mul(1_000_000_000u128) / self.sample_rate.max(1) as u128;
        let batch_duration = Duration::from_nanos(batch_ns.min(u64::MAX as u128) as u64);
        let deadline = self
            .next_rx_deadline
            .ok_or_else(|| Error::invalid_setting("RX stream is inactive"))?;
        let now = Instant::now();
        if deadline > now {
            let wait = deadline.duration_since(now);
            if timeout_us >= 0 && wait > Duration::from_micros(timeout_us as u64) {
                if timeout_us > 0 {
                    thread::sleep(Duration::from_micros(timeout_us as u64));
                }
                return Ok(RxReadResult {
                    samples_read: 0,
                    time_ticks: 0,
                    overflow: false,
                });
            }
            thread::sleep(wait);
        }
        self.next_rx_deadline = Some(deadline.max(now) + batch_duration);

        for sample in buf.iter_mut() {
            *sample = Complex32::new(0.0, 0.0);
        }

        let time_ticks = self.contiguous_time_ticks();
        self.samples_emitted = self.samples_emitted.saturating_add(buf.len() as u64);
        Ok(RxReadResult {
            samples_read: buf.len(),
            time_ticks,
            overflow: false,
        })
    }

    fn rx_activate(&mut self, time_ticks: Option<u64>) -> Result<(), Error> {
        if time_ticks.is_some() {
            return Err(Error::unsupported("timed RX activation"));
        }
        self.next_rx_deadline = Some(Instant::now());
        self.activation_ticks = self.clock_start.elapsed().as_nanos() as u64;
        self.samples_emitted = 0;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), Error> {
        self.next_rx_deadline = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const SAMPLE_RATE_HZ: usize = 1_000_000;
    const FUTURE_TICKS: u64 = 2_000_000_000;
    #[test]
    fn scheduled_write_accepts_before_start_and_rejects_past_time() {
        let (mut tx, _) = Box::new(NoopRadio::new(SAMPLE_RATE_HZ)).split().unwrap();
        tx.enable_transmit(true).unwrap();
        let target = tx.get_hardware_time().unwrap() + FUTURE_TICKS;
        let samples = [Complex32::new(0.1, 0.0); 32];
        let count = tx
            .write(
                &samples,
                crate::TxMetadata {
                    schedule: crate::Schedule::At(crate::DeviceTime::from_ticks(target)),
                    end_of_burst: false,
                },
            )
            .unwrap();
        assert_eq!(count, samples.len());
        assert!(tx.get_hardware_time().unwrap() < target);
        let error = tx
            .write(
                &samples,
                crate::TxMetadata {
                    schedule: crate::Schedule::At(crate::DeviceTime::from_ticks(0)),
                    end_of_burst: false,
                },
            )
            .unwrap_err();
        assert_eq!(error.kind(), crate::error::ErrorKind::Late);
        assert_eq!(error.accepted_samples(), Some(0));
        tx.stop().unwrap();
    }

    #[test]
    fn a_backend_without_an_independent_clock_rejects_a_rate_change() {
        let mut radio = NoopRadio::new(1_000_000);
        let current = radio.tick_rate();
        assert_eq!(radio.set_tick_rate(current).unwrap(), current);
        let error = radio.set_tick_rate(current / 2).unwrap_err();
        assert_eq!(error.kind(), crate::error::ErrorKind::Unsupported);
    }

    #[test]
    fn received_samples_are_timed_on_the_transmit_clock() {
        let mut radio = NoopRadio::new(SAMPLE_RATE_HZ);
        radio
            .setup_rx(0, "", SAMPLE_RATE_HZ as f64, SAMPLE_RATE_HZ as f64, None)
            .unwrap();
        let (tx, rx) = Box::new(radio).split().unwrap();
        let mut rx = rx.unwrap();
        thread::sleep(Duration::from_millis(20));
        let before = tx.get_hardware_time().unwrap();
        rx.rx_activate(None).unwrap();
        let mut buf = [Complex32::default(); 100];
        let first = rx.rx_read(&mut buf, 1_000_000).unwrap();
        let after = tx.get_hardware_time().unwrap();
        assert!(
            (before..=after).contains(&first.time_ticks),
            "RX started at {} outside TX time {before}..={after}",
            first.time_ticks
        );
        let second = rx.rx_read(&mut buf, 1_000_000).unwrap();
        assert_eq!(second.time_ticks - first.time_ticks, 100_000);
    }
}
