use num_complex::Complex32;

use crate::error::Error;

/// The result of a receive call. Only the first `samples_read` entries in the buffer are valid.
pub struct RxReadResult {
    /// Number of valid complex samples written into the caller's buffer.
    pub samples_read: usize,
    /// Time of the first sample, in RX clock ticks. Ignore this field when `samples_read` is zero.
    pub time_ticks: u64,
    /// Samples were lost to a hardware overflow or transport gap. This flag does not give the loss count.
    pub overflow: bool,
}

/// TX event counts since the backend started collecting them. A zero can also mean the driver does not report that event.
#[derive(Debug, Clone, Copy, Default)]
pub struct TxRadioHealth {
    /// Events where the device exhausted queued TX samples.
    pub underflows: u64,
    /// Events where a timed TX submission missed its deadline.
    pub late_packets: u64,
    /// Reported TX packet ordering or sequence faults.
    pub sequence_errors: u64,
    /// Reported acknowledgements of completed TX bursts.
    pub burst_acks: u64,
    /// Reported TX packets discarded by the backend.
    pub dropped_packets: u64,
    /// Reported TX events that do not fit the other counters.
    pub unknown_events: u64,
}

/// A radio device before its TX and RX streams are split.
///
/// Frequency, bandwidth, and sample rate settings are in Hz. Gain settings are in dB.
/// Drivers may round requested settings. Unsupported operations return errors,
/// including nonzero LO offsets on drivers without frequency compensation.
pub trait Radio: Send {
    /// TX capabilities available after splitting. Clock metadata is read from the actual handles.
    fn tx_capabilities(&self) -> crate::TxCapabilities {
        let capabilities = crate::TxCapabilities::default();
        if self.supports_timed_tx() {
            capabilities.with(crate::TxCapability::TimedSamples)
        } else {
            capabilities
        }
    }

    /// Last applied TX sample rate in Hz, or zero before a rate has been configured.
    fn tx_sample_rate_hz(&self) -> f64;
    /// Applied RX sample rate in Hz. `None` means no RX stream has been prepared.
    fn rx_sample_rate_hz(&self) -> Option<f64>;
    /// Whether TX samples can be scheduled by timestamp. Stream activation may still be immediate.
    fn supports_timed_tx(&self) -> bool {
        false
    }
    /// Ticks per second of the timestamp clock. This may differ from the sample rate.
    fn tick_rate(&self) -> u64;
    /// RX-to-TX timestamp correction in nanoseconds. See
    /// [`crate::RadioCalibration::rx_to_tx_delay_ns`] for the sign and application rules.
    fn rx_to_tx_delay_ns(&self) -> Option<i64> {
        None
    }
    /// Estimated connector power in dBm for unit-RMS TX IQ at the configured gain. `None` means uncalibrated.
    fn tx_full_scale_power_estimate_dbm(&self) -> Option<f32> {
        None
    }
    /// Estimated input power in dBm corresponding to 0 dBFS at the configured RX gain. `None` means uncalibrated.
    fn rx_reference_dbm(&self) -> Option<f32> {
        None
    }
    /// Sets the TX center frequency in Hz.
    fn set_tx_frequency(&mut self, center_frequency: usize) -> Result<(), Error>;
    /// Requests the TX IQ rate in Hz and returns the applied rate.
    fn set_tx_sample_rate(&mut self, sample_rate_hz: f64) -> Result<f64, Error>;
    /// Requests the TX analog filter bandwidth in Hz.
    fn set_tx_bandwidth(&mut self, bandwidth: usize) -> Result<(), Error>;
    /// Offsets the TX LO from the requested center in Hz, with backend frequency compensation.
    /// The default accepts zero and rejects nonzero offsets.
    fn set_tx_lo_offset_hz(&mut self, _offset_hz: i64) -> Result<(), Error> {
        if _offset_hz == 0 {
            Ok(())
        } else {
            Err(Error::unsupported("TX LO offset"))
        }
    }
    /// Requests the device clock rate that timestamps count in, returning the applied rate.
    /// Call it before any sample rate, because achievable rates follow this clock.
    /// The default accepts the current rate and rejects a change, which is what a backend
    /// whose clock follows the sample rate must do.
    fn set_tick_rate(&mut self, tick_rate: u64) -> Result<u64, Error> {
        if tick_rate == self.tick_rate() {
            Ok(tick_rate)
        } else {
            Err(Error::unsupported("device clock rate"))
        }
    }
    /// TX gain range in dB as `(minimum, maximum)`, or `None` when TX gain cannot be set.
    fn tx_gain_range_db(&self) -> Option<(f64, f64)> {
        None
    }
    /// Device time in ticks before splitting. The default returns an error, so the time
    /// is only known once the stream halves exist.
    fn hardware_time(&self) -> Result<u64, Error> {
        Err(Error::unsupported("device time before splitting"))
    }
    /// Current TX gain in dB, or `None` when TX gain cannot be read or set.
    fn tx_gain_db(&self) -> Option<f64> {
        None
    }
    /// Sets TX gain in dB and returns the applied gain. The default returns an error.
    fn set_tx_gain(&mut self, _gain_db: f64) -> Result<f64, Error> {
        Err(Error::unsupported("TX gain"))
    }
    /// Creates an RX stream without starting it and returns the actual sample rate in Hz.
    /// Call this before splitting, then tune the returned [`RadioRx`] before activation.
    /// Channel numbers start at zero. Use a port name accepted by the driver for
    /// `antenna`. A gain of `None` leaves the current gain unchanged.
    /// Backends without RX support return an error.
    fn setup_rx(
        &mut self,
        _channel: usize,
        _antenna: &str,
        _sample_rate_hz: f64,
        _bandwidth_hz: f64,
        _gain_db: Option<f64>,
    ) -> Result<f64, Error> {
        Err(Error::unsupported("RX"))
    }
    /// Takes ownership of the device and returns separate TX and RX handles.
    /// Call [`Self::setup_rx`] first to create the RX handle. Otherwise RX is `None`.
    fn split(self: Box<Self>) -> Result<(Box<dyn RadioTx>, Option<Box<dyn RadioRx>>), Error>;
}

fn validate_schedule(
    sample_count: usize,
    schedule: crate::Schedule,
    timed_supported: bool,
    rate_hz: f64,
    tick_rate: u64,
) -> Result<(), Error> {
    if let crate::Schedule::At(time) = schedule {
        if !timed_supported {
            return Err(Error::unsupported("timed TX"));
        }
        let rate = crate::stream::positive_hz(rate_hz, "TX sample rate")?;
        if tick_rate == 0 {
            return Err(Error::invalid_setting("TX clock rate is zero"));
        }
        let duration = sample_count as f64 * tick_rate as f64 / rate;
        if !duration.is_finite()
            || duration >= u64::MAX as f64
            || time.ticks().checked_add(duration.ceil() as u64).is_none()
        {
            return Err(Error::invalid_setting("TX timestamp range exceeds u64"));
        }
    }
    Ok(())
}

/// Device-time reader that can move to another thread without moving the TX stream.
pub trait RadioClock: Send {
    /// Returns ticks in the same epoch and rate as the stream that supplied this reader.
    fn get_hardware_time(&self) -> Result<u64, Error>;
}

/// A transmit stream that accepts complex IQ at the configured sample rate.
///
/// Filter and resample before sending samples here. Each component
/// reaches digital full scale near ±1, depending on the driver's sample format.
/// A timestamp gives the time of the first sample, in device clock ticks.
///
/// A successful write can return while samples are still queued. An error can occur
/// after some samples have been sent. Disable TX explicitly when you finish.
pub trait RadioTx: Send {
    /// Independent reader for moving blocking device-time reads off the stream owner.
    fn clock_reader(&self) -> Option<Box<dyn RadioClock>> {
        None
    }

    /// Reports independently supported timing and burst operations.
    fn capabilities(&self) -> crate::TxCapabilities {
        let capabilities = crate::TxCapabilities::default();
        if self.supports_timed_tx() {
            capabilities.with(crate::TxCapability::TimedSamples)
        } else {
            capabilities
        }
    }
    /// Writes a chunk and reports accepted samples. Success does not imply RF completion.
    /// A zero count for nonempty input indicates no progress. Errors may include known progress.
    fn write(
        &mut self,
        samples: &[Complex32],
        metadata: crate::TxMetadata,
    ) -> Result<usize, Error> {
        if metadata.end_of_burst && !self.capabilities().supports(crate::TxCapability::BurstEnd) {
            return Err(Error::unsupported("end-of-burst metadata"));
        }
        match self.transmit_at(samples, metadata.schedule.ticks()) {
            Ok(()) => {
                if metadata.end_of_burst {
                    self.end_burst()
                        .map_err(|error| error.with_accepted_samples(samples.len()))?;
                }
                Ok(samples.len())
            }
            Err(error) if error.kind() == crate::error::ErrorKind::IncompleteWrite => {
                error.accepted_samples().ok_or(error)
            }
            Err(error) => Err(error),
        }
    }
    /// Sends all samples, advancing a scheduled timestamp after each accepted prefix.
    /// The final marker is submitted only with the last sample or after a complete write.
    fn write_all(
        &mut self,
        samples: &[Complex32],
        metadata: crate::TxMetadata,
    ) -> Result<(), Error> {
        if metadata.end_of_burst && !self.capabilities().supports(crate::TxCapability::BurstEnd) {
            return Err(Error::unsupported("end-of-burst metadata"));
        }
        validate_schedule(
            samples.len(),
            metadata.schedule,
            self.capabilities()
                .supports(crate::TxCapability::TimedSamples),
            self.tx_sample_rate_hz(),
            self.tick_rate(),
        )?;
        let mut accepted = 0usize;
        while accepted < samples.len() {
            let last = metadata.end_of_burst && accepted == samples.len() - 1;
            let end = if metadata.end_of_burst && !last {
                samples.len() - 1
            } else {
                samples.len()
            };
            let schedule = match metadata.schedule {
                crate::Schedule::At(time) if accepted != 0 => {
                    let rate =
                        crate::stream::positive_hz(self.tx_sample_rate_hz(), "TX sample rate")?;
                    if self.tick_rate() == 0 {
                        return Err(Error::invalid_setting("TX clock rate is zero"));
                    }
                    let delta = (accepted as f64 * self.tick_rate() as f64 / rate).round();
                    if !delta.is_finite() || delta >= u64::MAX as f64 {
                        return Err(Error::invalid_setting("TX timestamp offset exceeds u64"));
                    }
                    let ticks = time
                        .ticks()
                        .checked_add(delta as u64)
                        .ok_or_else(|| Error::invalid_setting("TX timestamp exceeds u64"))?;
                    crate::Schedule::At(crate::DeviceTime::from_ticks(ticks))
                }
                schedule => schedule,
            };
            let count = self
                .write(
                    &samples[accepted..end],
                    crate::TxMetadata {
                        schedule,
                        end_of_burst: last,
                    },
                )
                .map_err(|error| {
                    let progress = error
                        .accepted_samples()
                        .map(|count| accepted.saturating_add(count));
                    match progress {
                        Some(count) => error.with_accepted_samples(count),
                        None => error,
                    }
                })?;
            if count == 0 || count > end - accepted {
                return Err(Error::incomplete_write(accepted, samples.len()));
            }
            accepted += count;
        }
        if samples.is_empty() && metadata.end_of_burst {
            self.end_burst()?;
        }
        Ok(())
    }
    /// Submits a complete scheduled burst. Activation is idempotent and resources remain ready.
    /// Returning success means samples and the end marker were accepted, not transmitted over RF.
    fn transmit_burst_at(
        &mut self,
        samples: &[Complex32],
        time: crate::DeviceTime,
    ) -> Result<(), Error> {
        let capabilities = self.capabilities();
        if !capabilities.supports(crate::TxCapability::TimedSamples)
            || !capabilities.supports(crate::TxCapability::BurstEnd)
        {
            return Err(Error::unsupported("scheduled bursts"));
        }
        if samples.is_empty() {
            return Err(Error::invalid_setting("a scheduled burst requires samples"));
        }
        validate_schedule(
            samples.len(),
            crate::Schedule::At(time),
            capabilities.supports(crate::TxCapability::TimedSamples),
            self.tx_sample_rate_hz(),
            self.tick_rate(),
        )?;
        self.enable_transmit(true)?;
        self.write_all(
            samples,
            crate::TxMetadata {
                schedule: crate::Schedule::At(time),
                end_of_burst: true,
            },
        )
    }
    /// Completes the current burst without shutting down native stream resources.
    fn end_burst(&mut self) -> Result<(), Error> {
        Err(Error::unsupported("ending a burst"))
    }
    /// Shuts down TX through the backend. A queued marker alone does not prove physical RF mute.
    fn stop(&mut self) -> Result<(), Error> {
        self.enable_transmit(false)
    }

    /// Applied TX sample rate in Hz. This may differ from the timestamp clock rate.
    fn tx_sample_rate_hz(&self) -> f64;
    /// RX-to-TX timestamp correction in nanoseconds. The HAL does not apply it.
    /// See [`crate::RadioCalibration::rx_to_tx_delay_ns`] for the scheduling equation.
    fn rx_to_tx_delay_ns(&self) -> Option<i64> {
        None
    }

    /// Estimated unit-RMS TX connector power in dBm at the calibrated gain. `None` means unavailable.
    fn tx_full_scale_power_estimate_dbm(&self) -> Option<f32> {
        None
    }

    /// Whether TX samples can be scheduled by timestamp. Stream activation may still be immediate.
    fn supports_timed_tx(&self) -> bool {
        false
    }
    /// Ticks per second of the timestamp clock. This may differ from the sample rate.
    fn tick_rate(&self) -> u64;
    /// Current device time in clock ticks. The clock origin depends on the backend, so this is not Unix time.
    fn get_hardware_time(&self) -> Result<u64, Error>;
    /// Resets the device clock to `ticks`, if supported. Existing scheduled timestamps then use the new epoch.
    /// The default implementation returns an unsupported-operation error.
    fn set_hardware_time(&self, _ticks: u64) -> Result<(), Error> {
        Err(Error::unsupported("setting hardware time"))
    }
    /// Submits consecutive final-rate IQ samples without an explicit start timestamp.
    fn transmit(&mut self, samples: &[Complex32]) -> Result<(), Error>;
    /// Reserves working memory for blocks up to `max_samples` where the backend supports it.
    /// Larger writes are still allowed. The default does nothing.
    fn prepare_transmit(&mut self, _max_samples: usize) -> Result<(), Error> {
        Ok(())
    }
    /// Schedules the first sample at `tick`. Use `None` to send without a timestamp.
    /// The default rejects timed writes before sending samples. Drivers differ in
    /// how they handle a timestamp that has already passed.
    fn transmit_at(&mut self, samples: &[Complex32], tick: Option<u64>) -> Result<(), Error> {
        if tick.is_some() {
            return Err(Error::unsupported("timed TX"));
        }
        self.transmit(samples)
    }
    /// Enables or stops the TX stream. Software backends may treat this as pacing control or a no-op.
    fn enable_transmit(&mut self, enable: bool) -> Result<(), Error>;
    /// Starts or stops TX, optionally at a device timestamp.
    /// The default rejects timestamps. Drivers must explicitly implement timed activation.
    fn enable_transmit_at(&mut self, enable: bool, tick: Option<u64>) -> Result<(), Error> {
        if tick.is_some() {
            return Err(Error::unsupported("timed TX activation"));
        }
        self.enable_transmit(enable)
    }
    /// Returns cumulative backend TX counters. The default returns zero for every counter.
    fn tx_health(&mut self) -> Result<TxRadioHealth, Error> {
        Ok(TxRadioHealth::default())
    }
    /// Retunes TX in Hz after splitting, if supported. The default returns an error.
    fn set_tx_frequency_hz(&mut self, _frequency_hz: f64) -> Result<(), Error> {
        Err(Error::unsupported("TX retuning"))
    }
    /// Sets TX gain in dB after splitting and returns the applied gain, if supported.
    /// The default returns an error.
    fn set_tx_gain(&mut self, _gain_db: f64) -> Result<f64, Error> {
        Err(Error::unsupported("TX gain"))
    }
}

/// A receive stream. Activate it before reading and deactivate it when you finish.
///
/// Unsupported scheduled activation returns an error.
/// Use the first read timestamp to find the actual start. A read can return fewer
/// samples than the buffer holds, or zero when it times out.
pub trait RadioRx: Send {
    /// Reads using a portable wait request. Native adapters reject unsupported wait modes.
    fn read(
        &mut self,
        buf: &mut [Complex32],
        timeout: crate::ReadTimeout,
    ) -> Result<RxReadResult, Error> {
        let result = self.rx_read(buf, timeout.microseconds()?)?;
        if result.samples_read > buf.len() {
            return Err(Error::new(
                crate::error::ErrorKind::Backend,
                "RX count exceeds buffer capacity",
            ));
        }
        Ok(result)
    }

    /// Applied RX sample rate in Hz, including fractional driver readback.
    fn rx_sample_rate_hz(&self) -> f64;
    /// Estimated input power in dBm corresponding to 0 dBFS at the calibrated gain. `None` means unavailable.
    fn rx_reference_dbm(&self) -> Option<f32> {
        None
    }

    /// Ticks per second of the timestamp clock. This may differ from the sample rate.
    fn tick_rate(&self) -> u64;
    /// Current device time in clock ticks. The clock origin depends on the backend, so this is not Unix time.
    fn get_hardware_time(&self) -> Result<u64, Error>;
    /// Fills `buf` with received IQ and returns the sample count and time of the first sample.
    /// `timeout_us` sets the driver wait time in microseconds. The call can take longer.
    /// The meaning of zero and negative timeouts depends on the backend.
    fn rx_read(&mut self, buf: &mut [Complex32], timeout_us: i64) -> Result<RxReadResult, Error>;
    /// Requests receive activation at an optional device tick. Unsupported timing returns an error.
    fn rx_activate(&mut self, time_ticks: Option<u64>) -> Result<(), Error>;
    /// Stops the receive stream. A later activation can resume it with backend-specific buffering.
    fn stop(&mut self) -> Result<(), Error>;
    /// Retunes RX in Hz after splitting, if supported. The default returns an error.
    fn set_rx_frequency(&mut self, _frequency_hz: f64) -> Result<(), Error> {
        Err(Error::unsupported("RX retuning"))
    }
    /// Sets RX gain in dB after splitting, if supported. The default returns an error.
    fn set_rx_gain(&mut self, _gain_db: f64) -> Result<(), Error> {
        Err(Error::unsupported("RX gain"))
    }
}

impl RadioTx for Box<dyn RadioTx> {
    fn clock_reader(&self) -> Option<Box<dyn RadioClock>> {
        (**self).clock_reader()
    }

    fn capabilities(&self) -> crate::TxCapabilities {
        (**self).capabilities()
    }
    fn write(
        &mut self,
        samples: &[Complex32],
        metadata: crate::TxMetadata,
    ) -> Result<usize, Error> {
        (**self).write(samples, metadata)
    }
    fn write_all(
        &mut self,
        samples: &[Complex32],
        metadata: crate::TxMetadata,
    ) -> Result<(), Error> {
        (**self).write_all(samples, metadata)
    }
    fn transmit_burst_at(
        &mut self,
        samples: &[Complex32],
        time: crate::DeviceTime,
    ) -> Result<(), Error> {
        (**self).transmit_burst_at(samples, time)
    }
    fn end_burst(&mut self) -> Result<(), Error> {
        (**self).end_burst()
    }
    fn stop(&mut self) -> Result<(), Error> {
        (**self).stop()
    }

    fn tx_sample_rate_hz(&self) -> f64 {
        (**self).tx_sample_rate_hz()
    }
    fn rx_to_tx_delay_ns(&self) -> Option<i64> {
        (**self).rx_to_tx_delay_ns()
    }
    fn tx_full_scale_power_estimate_dbm(&self) -> Option<f32> {
        (**self).tx_full_scale_power_estimate_dbm()
    }

    fn supports_timed_tx(&self) -> bool {
        (**self).supports_timed_tx()
    }
    fn tick_rate(&self) -> u64 {
        (**self).tick_rate()
    }
    fn get_hardware_time(&self) -> Result<u64, Error> {
        (**self).get_hardware_time()
    }
    fn set_hardware_time(&self, ticks: u64) -> Result<(), Error> {
        (**self).set_hardware_time(ticks)
    }
    fn transmit(&mut self, samples: &[Complex32]) -> Result<(), Error> {
        (**self).transmit(samples)
    }
    fn prepare_transmit(&mut self, max_samples: usize) -> Result<(), Error> {
        (**self).prepare_transmit(max_samples)
    }
    fn transmit_at(&mut self, samples: &[Complex32], tick: Option<u64>) -> Result<(), Error> {
        (**self).transmit_at(samples, tick)
    }
    fn enable_transmit(&mut self, enable: bool) -> Result<(), Error> {
        (**self).enable_transmit(enable)
    }
    fn enable_transmit_at(&mut self, enable: bool, tick: Option<u64>) -> Result<(), Error> {
        (**self).enable_transmit_at(enable, tick)
    }
    fn tx_health(&mut self) -> Result<TxRadioHealth, Error> {
        (**self).tx_health()
    }
    fn set_tx_frequency_hz(&mut self, frequency_hz: f64) -> Result<(), Error> {
        (**self).set_tx_frequency_hz(frequency_hz)
    }
    fn set_tx_gain(&mut self, gain_db: f64) -> Result<f64, Error> {
        (**self).set_tx_gain(gain_db)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct UntimedTx {
        writes: usize,
    }
    impl RadioTx for UntimedTx {
        fn tx_sample_rate_hz(&self) -> f64 {
            1.0
        }
        fn tick_rate(&self) -> u64 {
            1_000_000_000
        }
        fn get_hardware_time(&self) -> Result<u64, Error> {
            Ok(0)
        }
        fn transmit(&mut self, _samples: &[Complex32]) -> Result<(), Error> {
            self.writes += 1;
            Ok(())
        }
        fn enable_transmit(&mut self, _enable: bool) -> Result<(), Error> {
            Ok(())
        }
    }

    #[test]
    fn unsupported_timed_tx_does_not_emit_samples() {
        let mut tx = UntimedTx { writes: 0 };
        assert!(!tx.supports_timed_tx());
        assert!(
            tx.transmit_at(&[Complex32::new(1.0, 0.0)], Some(42))
                .is_err()
        );
        assert_eq!(tx.writes, 0);
        assert!(tx.enable_transmit_at(true, Some(42)).is_err());
        tx.transmit_at(&[Complex32::new(1.0, 0.0)], None).unwrap();
        assert_eq!(tx.writes, 1);
    }
    struct ShortTx {
        writes: Vec<(usize, crate::TxMetadata)>,
        enabled: usize,
        limit: usize,
        fail_after: Option<usize>,
    }
    impl RadioTx for ShortTx {
        fn capabilities(&self) -> crate::TxCapabilities {
            crate::TxCapabilities::from([
                crate::TxCapability::TimedSamples,
                crate::TxCapability::BurstEnd,
            ])
        }
        fn tx_sample_rate_hz(&self) -> f64 {
            2.5
        }
        fn tick_rate(&self) -> u64 {
            100
        }
        fn get_hardware_time(&self) -> Result<u64, Error> {
            Ok(0)
        }
        fn transmit(&mut self, _: &[Complex32]) -> Result<(), Error> {
            unreachable!()
        }
        fn enable_transmit(&mut self, enable: bool) -> Result<(), Error> {
            self.enabled += usize::from(enable);
            Ok(())
        }
        fn write(
            &mut self,
            samples: &[Complex32],
            metadata: crate::TxMetadata,
        ) -> Result<usize, Error> {
            if self.fail_after == Some(self.writes.len()) {
                return Err(Error::with_source(
                    crate::error::ErrorKind::Timeout,
                    "native write failed",
                    std::io::Error::from(std::io::ErrorKind::TimedOut),
                )
                .with_accepted_samples(1));
            }
            let count = samples.len().min(self.limit);
            self.writes.push((count, metadata));
            Ok(count)
        }
    }
    fn short_tx(limit: usize) -> ShortTx {
        ShortTx {
            writes: Vec::new(),
            enabled: 0,
            limit,
            fail_after: None,
        }
    }
    fn burst_metadata(ticks: u64) -> crate::TxMetadata {
        crate::TxMetadata {
            schedule: crate::Schedule::At(crate::DeviceTime::from_ticks(ticks)),
            end_of_burst: true,
        }
    }
    #[test]
    fn short_writes_advance_fractional_rate_timestamps_and_mark_only_final_sample() {
        let mut tx = short_tx(2);
        tx.write_all(&[Complex32::default(); 6], burst_metadata(1000))
            .unwrap();
        assert_eq!(
            tx.writes
                .iter()
                .map(|(n, m)| (*n, m.schedule.ticks(), m.end_of_burst))
                .collect::<Vec<_>>(),
            vec![
                (2, Some(1000), false),
                (2, Some(1080), false),
                (1, Some(1160), false),
                (1, Some(1200), true)
            ]
        );
        assert_eq!(tx.enabled, 0);
    }
    #[test]
    fn no_progress_and_partial_failures_report_known_total() {
        let samples = [Complex32::default(); 6];
        let mut tx = short_tx(0);
        let error = tx.write_all(&samples, burst_metadata(0)).unwrap_err();
        assert_eq!(error.kind(), crate::error::ErrorKind::IncompleteWrite);
        assert_eq!(error.accepted_samples(), Some(0));
        let mut tx = short_tx(2);
        tx.fail_after = Some(1);
        let error = tx.write_all(&samples, burst_metadata(0)).unwrap_err();
        assert_eq!(error.kind(), crate::error::ErrorKind::Timeout);
        assert_eq!(error.accepted_samples(), Some(3));
        assert!(std::error::Error::source(&error).is_some());
    }
    #[test]
    fn invalid_bursts_and_timestamp_ranges_have_no_side_effects() {
        let mut tx = short_tx(2);
        assert!(
            tx.transmit_burst_at(&[], crate::DeviceTime::from_ticks(0))
                .is_err()
        );
        assert_eq!(tx.enabled, 0);
        assert!(
            tx.transmit_burst_at(
                &[Complex32::default(); 2],
                crate::DeviceTime::from_ticks(u64::MAX)
            )
            .is_err()
        );
        assert_eq!(tx.enabled, 0);

        assert!(
            tx.write_all(&[Complex32::default(); 2], burst_metadata(u64::MAX))
                .is_err()
        );
        assert!(tx.writes.is_empty());
    }
}
