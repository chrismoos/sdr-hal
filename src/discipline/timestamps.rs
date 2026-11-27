//! Stream halves of a radio whose oscillator is already locked to the reference.
//!
//! The pulses give the time of each device tick and nothing else, so samples and
//! timestamps pass through unchanged.

use std::sync::Arc;

use num_complex::Complex32;

use super::Shared;
use super::tx::DisciplinedClock;
use crate::error::Error;
use crate::{RadioClock, RadioRx, RadioTx, RxReadResult, TxCapabilities, TxCapability, TxMetadata};

pub(crate) struct TimestampedTx {
    pub(crate) inner: Box<dyn RadioTx>,
    pub(crate) shared: Arc<Shared>,
}

impl RadioTx for TimestampedTx {
    fn clock_reader(&self) -> Option<Box<dyn RadioClock>> {
        let inner = self.inner.clock_reader()?;
        Some(Box::new(DisciplinedClock {
            inner,
            shared: Arc::clone(&self.shared),
        }))
    }

    fn time_reference(&self) -> Result<Option<crate::TimeReference>, Error> {
        Ok(Some(self.shared.status.time_reference()))
    }

    fn clock_status(&self) -> Option<crate::ClockStatus> {
        Some(self.shared.status.status())
    }

    fn capabilities(&self) -> TxCapabilities {
        // Resetting the device clock would move every pulse the reference is anchored to.
        let inner = self.inner.capabilities();
        [
            TxCapability::TimedSamples,
            TxCapability::TimedActivation,
            TxCapability::BurstEnd,
        ]
        .into_iter()
        .filter(|capability| inner.supports(*capability))
        .fold(TxCapabilities::default(), TxCapabilities::with)
    }

    fn write(&mut self, samples: &[Complex32], metadata: TxMetadata) -> Result<usize, Error> {
        self.inner.write(samples, metadata)
    }

    fn end_burst(&mut self) -> Result<(), Error> {
        self.inner.end_burst()
    }

    fn tx_sample_rate_hz(&self) -> f64 {
        self.inner.tx_sample_rate_hz()
    }

    fn rx_to_tx_delay_ns(&self) -> Option<i64> {
        self.inner.rx_to_tx_delay_ns()
    }

    fn tx_full_scale_power_estimate_dbm(&self) -> Option<f32> {
        self.inner.tx_full_scale_power_estimate_dbm()
    }

    fn supports_timed_tx(&self) -> bool {
        self.inner.supports_timed_tx()
    }

    fn tick_rate(&self) -> u64 {
        self.inner.tick_rate()
    }

    fn get_hardware_time(&self) -> Result<u64, Error> {
        self.inner.get_hardware_time()
    }

    fn set_hardware_time(&self, _ticks: u64) -> Result<(), Error> {
        Err(Error::unsupported(
            "setting the time of a clock tracking a reference",
        ))
    }

    fn transmit(&mut self, samples: &[Complex32]) -> Result<(), Error> {
        self.inner.transmit(samples)
    }

    fn prepare_transmit(&mut self, max_samples: usize) -> Result<(), Error> {
        self.inner.prepare_transmit(max_samples)
    }

    fn transmit_at(&mut self, samples: &[Complex32], tick: Option<u64>) -> Result<(), Error> {
        self.inner.transmit_at(samples, tick)
    }

    fn enable_transmit(&mut self, enable: bool) -> Result<(), Error> {
        self.inner.enable_transmit(enable)
    }

    fn enable_transmit_at(&mut self, enable: bool, tick: Option<u64>) -> Result<(), Error> {
        self.inner.enable_transmit_at(enable, tick)
    }

    fn tx_health(&mut self) -> Result<crate::TxRadioHealth, Error> {
        self.inner.tx_health()
    }

    fn set_tx_frequency_hz(&mut self, frequency_hz: f64) -> Result<(), Error> {
        self.inner.set_tx_frequency_hz(frequency_hz)
    }

    fn set_tx_gain(&mut self, gain_db: f64) -> Result<f64, Error> {
        self.inner.set_tx_gain(gain_db)
    }
}

pub(crate) struct TimestampedRx {
    pub(crate) inner: Box<dyn RadioRx>,
    /// Keeps the reference tracking running for as long as either half is open.
    pub(crate) _shared: Arc<Shared>,
}

impl RadioRx for TimestampedRx {
    fn rx_sample_rate_hz(&self) -> f64 {
        self.inner.rx_sample_rate_hz()
    }

    fn rx_reference_dbm(&self) -> Option<f32> {
        self.inner.rx_reference_dbm()
    }

    fn tick_rate(&self) -> u64 {
        self.inner.tick_rate()
    }

    fn get_hardware_time(&self) -> Result<u64, Error> {
        self.inner.get_hardware_time()
    }

    fn rx_read(&mut self, buf: &mut [Complex32], timeout_us: i64) -> Result<RxReadResult, Error> {
        self.inner.rx_read(buf, timeout_us)
    }

    fn rx_activate(&mut self, time_ticks: Option<u64>) -> Result<(), Error> {
        self.inner.rx_activate(time_ticks)
    }

    fn stop(&mut self) -> Result<(), Error> {
        self.inner.stop()
    }

    fn set_rx_frequency(&mut self, frequency_hz: f64) -> Result<(), Error> {
        self.inner.set_rx_frequency(frequency_hz)
    }

    fn set_rx_gain(&mut self, gain_db: f64) -> Result<(), Error> {
        self.inner.set_rx_gain(gain_db)
    }
}
