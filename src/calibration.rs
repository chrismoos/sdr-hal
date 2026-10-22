use num_complex::Complex32;

use crate::{Radio, RadioRx, RadioTx, RxReadResult, TxRadioHealth, error::Error};

/// Power and timing calibration for a radio. `None` means a value is unknown.
/// Power references apply at the gain used for calibration. Update them if you change gain.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RadioCalibration {
    /// Correction from an RX-relative target time to a TX scheduling timestamp.
    /// Add this signed value: `tx_time_ns = rx_target_time_ns + rx_to_tx_delay_ns`.
    /// For an RX target of 1,000,000 ns, +200 schedules TX at 1,000,200 ns and
    /// -200 schedules it at 999,800 ns. Convert the result to TX clock ticks using
    /// [`RadioTx::tick_rate`] before calling [`RadioTx::transmit_at`].
    ///
    /// This is calibration metadata. The HAL does not apply it to timestamps,
    /// insert silence, shift IQ, or delay a thread. Apply it once in your scheduler.
    /// It is not network latency or a lead-time allowance for submitting samples.
    /// RX and TX must share a clock or have a known clock mapping.
    /// `None` means unknown. `Some(0)` explicitly overrides a backend correction.
    /// The value is independent of sample rate and remains attached after splitting.
    pub rx_to_tx_delay_ns: Option<i64>,
    /// Connector power in dBm for unit-RMS TX IQ at the calibrated gain.
    pub tx_full_scale_power_estimate_dbm: Option<f32>,
    /// Input power in dBm corresponding to 0 dBFS at the calibrated RX gain.
    pub rx_reference_dbm: Option<f32>,
}

/// A radio with calibration overrides. Each supplied value replaces the backend
/// value. Fields left as `None` keep the backend calibration. Both split handles
/// retain their calibration.
pub struct CalibratedRadio {
    inner: Box<dyn Radio>,
    calibration: RadioCalibration,
    tx_estimate: TxEstimate,
    rx_reference_valid: bool,
    rx_reference_gain_db: Option<f64>,
}

/// A TX power estimate override and the gain it currently applies at.
///
/// Connector power tracks TX gain in dB, so a known gain moves the estimate with it. An
/// estimate whose gain is unknown cannot follow a change and is withdrawn instead.
#[derive(Clone, Copy, Debug)]
struct TxEstimate {
    dbm: Option<f32>,
    gain_db: Option<f64>,
}

impl TxEstimate {
    fn follow_gain(&mut self, applied_gain_db: f64) {
        self.dbm = match (self.dbm, self.gain_db) {
            (Some(dbm), Some(gain_db)) => Some(dbm + (applied_gain_db - gain_db) as f32),
            _ => None,
        };
        self.gain_db = Some(applied_gain_db);
    }

    fn or_backend(&self, backend: Option<f32>, overridden: bool) -> Option<f32> {
        if overridden { self.dbm } else { backend }
    }
}

impl CalibratedRadio {
    /// Wraps a radio without changing its hardware settings.
    /// Power references must be finite. NaN and infinity return an error.
    pub fn new(inner: Box<dyn Radio>, calibration: RadioCalibration) -> Result<Self, Error> {
        if calibration
            .tx_full_scale_power_estimate_dbm
            .is_some_and(|value| !value.is_finite())
            || calibration
                .rx_reference_dbm
                .is_some_and(|value| !value.is_finite())
        {
            return Err(Error::invalid_setting(
                "radio power calibration must be finite",
            ));
        }
        Ok(Self {
            inner,
            calibration,
            tx_estimate: TxEstimate {
                dbm: calibration.tx_full_scale_power_estimate_dbm,
                gain_db: None,
            },
            rx_reference_valid: true,
            rx_reference_gain_db: None,
        })
    }
    /// Records the TX gain, in dB, at which the supplied `tx_full_scale_power_estimate_dbm`
    /// override applies. Hardware gain is unchanged. A later gain change moves the estimate
    /// by the same number of dB. With `None`, any gain change withdraws the override.
    pub fn with_tx_reference_gain_db(mut self, gain_db: Option<f64>) -> Result<Self, Error> {
        if let Some(value) = gain_db {
            crate::stream::finite(value, "TX reference gain")?;
        }
        self.tx_estimate.gain_db = gain_db;
        Ok(self)
    }
    /// Records the RX gain, in dB, at which the supplied `rx_reference_dbm` override was measured.
    /// Hardware gain is unchanged. Changing it invalidates the reference until the measured gain is restored.
    /// With `None`, any explicit gain change invalidates the override. Without an override, the backend manages its reference.
    pub fn with_rx_reference_gain_db(mut self, gain_db: Option<f64>) -> Result<Self, Error> {
        if let Some(value) = gain_db {
            crate::stream::finite(value, "RX reference gain")?;
        }
        self.rx_reference_gain_db = gain_db;
        Ok(self)
    }
}

impl Radio for CalibratedRadio {
    fn tx_capabilities(&self) -> crate::TxCapabilities {
        self.inner.tx_capabilities()
    }

    fn tx_sample_rate_hz(&self) -> f64 {
        self.inner.tx_sample_rate_hz()
    }
    fn rx_sample_rate_hz(&self) -> Option<f64> {
        self.inner.rx_sample_rate_hz()
    }
    fn rx_to_tx_delay_ns(&self) -> Option<i64> {
        self.calibration
            .rx_to_tx_delay_ns
            .or_else(|| self.inner.rx_to_tx_delay_ns())
    }
    fn tx_full_scale_power_estimate_dbm(&self) -> Option<f32> {
        self.tx_estimate.or_backend(
            self.inner.tx_full_scale_power_estimate_dbm(),
            self.calibration.tx_full_scale_power_estimate_dbm.is_some(),
        )
    }
    fn tx_gain_range_db(&self) -> Option<(f64, f64)> {
        self.inner.tx_gain_range_db()
    }
    fn tx_gain_db(&self) -> Option<f64> {
        self.inner.tx_gain_db()
    }
    fn hardware_time(&self) -> Result<u64, Error> {
        self.inner.hardware_time()
    }
    fn set_tx_gain(&mut self, gain_db: f64) -> Result<f64, Error> {
        let applied = self.inner.set_tx_gain(gain_db)?;
        self.tx_estimate.follow_gain(applied);
        Ok(applied)
    }
    fn rx_reference_dbm(&self) -> Option<f32> {
        match self.calibration.rx_reference_dbm {
            Some(reference) => self.rx_reference_valid.then_some(reference),
            None => self.inner.rx_reference_dbm(),
        }
    }
    fn split(self: Box<Self>) -> Result<(Box<dyn RadioTx>, Option<Box<dyn RadioRx>>), Error> {
        let calibration = RadioCalibration {
            rx_to_tx_delay_ns: self.rx_to_tx_delay_ns(),
            tx_full_scale_power_estimate_dbm: self.calibration.tx_full_scale_power_estimate_dbm,
            rx_reference_dbm: self.calibration.rx_reference_dbm,
        };
        let tx_estimate = self.tx_estimate;
        let (tx, rx) = self.inner.split()?;
        Ok((
            Box::new(CalibratedTx {
                inner: tx,
                calibration,
                tx_estimate,
            }),
            rx.map(|inner| {
                Box::new(CalibratedRx {
                    inner,
                    calibration,
                    reference_valid: self.rx_reference_valid,
                    reference_gain_db: self.rx_reference_gain_db,
                }) as Box<dyn RadioRx>
            }),
        ))
    }
    fn supports_timed_tx(&self) -> bool {
        self.inner.supports_timed_tx()
    }
    fn tick_rate(&self) -> u64 {
        self.inner.tick_rate()
    }
    fn set_tick_rate(&mut self, tick_rate: u64) -> Result<u64, Error> {
        self.inner.set_tick_rate(tick_rate)
    }
    fn set_tx_frequency(&mut self, frequency_hz: usize) -> Result<(), Error> {
        self.inner.set_tx_frequency(frequency_hz)
    }
    fn set_tx_sample_rate(&mut self, sample_rate_hz: f64) -> Result<f64, Error> {
        self.inner.set_tx_sample_rate(sample_rate_hz)
    }
    fn set_tx_bandwidth(&mut self, bandwidth_hz: usize) -> Result<(), Error> {
        self.inner.set_tx_bandwidth(bandwidth_hz)
    }
    fn set_tx_lo_offset_hz(&mut self, offset_hz: i64) -> Result<(), Error> {
        self.inner.set_tx_lo_offset_hz(offset_hz)
    }
    fn setup_rx(
        &mut self,
        channel: usize,
        antenna: &str,
        sample_rate_hz: f64,
        bandwidth_hz: f64,
        gain_db: Option<f64>,
    ) -> Result<f64, Error> {
        let actual =
            self.inner
                .setup_rx(channel, antenna, sample_rate_hz, bandwidth_hz, gain_db)?;
        if let Some(gain) = gain_db {
            self.rx_reference_valid = self.rx_reference_gain_db == Some(gain);
        }
        Ok(actual)
    }
}

struct CalibratedTx {
    inner: Box<dyn RadioTx>,
    calibration: RadioCalibration,
    tx_estimate: TxEstimate,
}
impl RadioTx for CalibratedTx {
    fn clock_reader(&self) -> Option<Box<dyn crate::RadioClock>> {
        self.inner.clock_reader()
    }

    fn capabilities(&self) -> crate::TxCapabilities {
        self.inner.capabilities()
    }
    fn write(
        &mut self,
        samples: &[Complex32],
        metadata: crate::TxMetadata,
    ) -> Result<usize, Error> {
        self.inner.write(samples, metadata)
    }
    fn write_all(
        &mut self,
        samples: &[Complex32],
        metadata: crate::TxMetadata,
    ) -> Result<(), Error> {
        self.inner.write_all(samples, metadata)
    }
    fn transmit_burst_at(
        &mut self,
        samples: &[Complex32],
        time: crate::DeviceTime,
    ) -> Result<(), Error> {
        self.inner.transmit_burst_at(samples, time)
    }
    fn end_burst(&mut self) -> Result<(), Error> {
        self.inner.end_burst()
    }
    fn stop(&mut self) -> Result<(), Error> {
        self.inner.stop()
    }

    fn tx_sample_rate_hz(&self) -> f64 {
        self.inner.tx_sample_rate_hz()
    }
    fn rx_to_tx_delay_ns(&self) -> Option<i64> {
        self.calibration
            .rx_to_tx_delay_ns
            .or_else(|| self.inner.rx_to_tx_delay_ns())
    }
    fn tx_full_scale_power_estimate_dbm(&self) -> Option<f32> {
        self.tx_estimate.or_backend(
            self.inner.tx_full_scale_power_estimate_dbm(),
            self.calibration.tx_full_scale_power_estimate_dbm.is_some(),
        )
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
    fn set_hardware_time(&self, ticks: u64) -> Result<(), Error> {
        self.inner.set_hardware_time(ticks)
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
    fn tx_health(&mut self) -> Result<TxRadioHealth, Error> {
        self.inner.tx_health()
    }
    fn set_tx_frequency_hz(&mut self, frequency_hz: f64) -> Result<(), Error> {
        self.inner.set_tx_frequency_hz(frequency_hz)
    }
    fn set_tx_gain(&mut self, gain_db: f64) -> Result<f64, Error> {
        let applied = self.inner.set_tx_gain(gain_db)?;
        self.tx_estimate.follow_gain(applied);
        Ok(applied)
    }
}

struct CalibratedRx {
    reference_valid: bool,
    reference_gain_db: Option<f64>,
    inner: Box<dyn RadioRx>,
    calibration: RadioCalibration,
}
impl RadioRx for CalibratedRx {
    fn rx_sample_rate_hz(&self) -> f64 {
        self.inner.rx_sample_rate_hz()
    }
    fn rx_reference_dbm(&self) -> Option<f32> {
        match self.calibration.rx_reference_dbm {
            Some(reference) => self.reference_valid.then_some(reference),
            None => self.inner.rx_reference_dbm(),
        }
    }
    fn tick_rate(&self) -> u64 {
        self.inner.tick_rate()
    }
    fn get_hardware_time(&self) -> Result<u64, Error> {
        self.inner.get_hardware_time()
    }
    fn read(
        &mut self,
        buf: &mut [Complex32],
        timeout: crate::ReadTimeout,
    ) -> Result<RxReadResult, Error> {
        self.inner.read(buf, timeout)
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
        crate::stream::finite(gain_db, "RX gain")?;
        self.inner.set_rx_gain(gain_db)?;
        self.reference_valid = self.reference_gain_db == Some(gain_db);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::TxEstimate;

    #[test]
    fn a_tx_estimate_moves_with_gain_and_is_withdrawn_without_a_reference() {
        let mut known = TxEstimate {
            dbm: Some(10.0),
            gain_db: Some(60.0),
        };
        known.follow_gain(50.0);
        assert_eq!(known.dbm, Some(0.0));
        known.follow_gain(55.5);
        assert_eq!(known.dbm, Some(5.5));

        let mut unknown = TxEstimate {
            dbm: Some(10.0),
            gain_db: None,
        };
        unknown.follow_gain(50.0);
        assert_eq!(unknown.dbm, None);
        assert_eq!(unknown.or_backend(Some(-3.0), true), None);
        assert_eq!(unknown.or_backend(Some(-3.0), false), Some(-3.0));
    }
}
