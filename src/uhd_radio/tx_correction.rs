//! Absolute device ticks determine correction phase, so discarded writes cannot shift later samples.

use num_complex::Complex32;
use uhd::{TuneRequest, TuneRequestPolicy};

use crate::Schedule;
use crate::dsp::PhasorNco;
use crate::error::Error;

pub(super) enum TxFrequencyCorrection {
    Hardware,
    Software {
        correction_hz: Option<f64>,
        buffer: Vec<Complex32>,
    },
}

impl Default for TxFrequencyCorrection {
    fn default() -> Self {
        Self::software()
    }
}

impl TxFrequencyCorrection {
    pub(super) fn software() -> Self {
        Self::Software {
            correction_hz: None,
            buffer: Vec::new(),
        }
    }

    pub(super) fn is_software(&self) -> bool {
        matches!(self, Self::Software { .. })
    }

    pub(super) fn tune_request(
        &self,
        carrier_hz: f64,
        lo_offset_hz: f64,
    ) -> Result<TuneRequest, Error> {
        if self.is_software() {
            if lo_offset_hz != 0.0 {
                return Err(Error::invalid_setting(
                    "software UHD TX frequency correction requires zero TX LO offset",
                ));
            }
            let mut request = TuneRequest::with_frequency(carrier_hz);
            request.set_dsp_policy(TuneRequestPolicy::Manual(0.0));
            Ok(request)
        } else if lo_offset_hz != 0.0 {
            Ok(TuneRequest::with_frequency_lo(carrier_hz, lo_offset_hz))
        } else {
            Ok(TuneRequest::with_frequency(carrier_hz))
        }
    }

    pub(super) fn update_tune(
        &mut self,
        requested_hz: f64,
        actual_rf_hz: f64,
        actual_dsp_hz: f64,
    ) -> Result<(), Error> {
        if let Self::Software { correction_hz, .. } = self {
            *correction_hz = None;
            crate::stream::positive_hz(requested_hz, "requested TX carrier")?;
            crate::stream::positive_hz(actual_rf_hz, "actual TX RF frequency")?;
            if actual_dsp_hz != 0.0 {
                return Err(Error::invalid_setting(format!(
                    "software UHD TX frequency correction requires zero hardware DSP frequency, got {actual_dsp_hz} Hz"
                )));
            }
            *correction_hz = Some(requested_hz - actual_rf_hz);
            log::info!(
                "UHD: software TX frequency correction requested_hz={requested_hz} actual_rf_hz={actual_rf_hz} hardware_dsp_hz={actual_dsp_hz} correction_hz={}",
                requested_hz - actual_rf_hz
            );
        }
        Ok(())
    }

    pub(super) fn prepare(&mut self, max_samples: usize) {
        if let Self::Software { buffer, .. } = self
            && max_samples > buffer.capacity()
        {
            buffer.reserve(max_samples - buffer.len());
        }
    }

    pub(super) fn samples<'a>(
        &'a mut self,
        input: &'a [Complex32],
        schedule: Schedule,
        sample_rate_hz: f64,
        tick_rate: u64,
    ) -> Result<&'a [Complex32], Error> {
        let Self::Software {
            correction_hz,
            buffer,
        } = self
        else {
            return Ok(input);
        };
        let Schedule::At(time) = schedule else {
            return Err(Error::unsupported(
                "untimed TX with software UHD frequency correction",
            ));
        };
        let correction_hz = correction_hz.ok_or_else(|| {
            Error::invalid_setting("tune TX before using software UHD frequency correction")
        })?;
        crate::stream::positive_hz(sample_rate_hz, "TX sample rate")?;
        if tick_rate == 0 {
            return Err(Error::invalid_setting("TX tick rate must be positive"));
        }
        if correction_hz.abs() >= sample_rate_hz / 2.0 {
            return Err(Error::invalid_setting(format!(
                "software TX correction {correction_hz} Hz reaches Nyquist at {sample_rate_hz} samples/s"
            )));
        }
        let seconds = time.ticks() / tick_rate;
        let remainder = time.ticks() % tick_rate;
        let cycles = (correction_hz * seconds as f64).fract()
            + correction_hz * (remainder as f64 / tick_rate as f64);
        let phase = std::f64::consts::TAU * cycles.rem_euclid(1.0);
        let step = std::f64::consts::TAU * correction_hz / sample_rate_hz;
        let mut oscillator = PhasorNco::with_start_phase(phase, step);
        oscillator.rotate_into(input, buffer);
        Ok(buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DeviceTime;

    const CARRIER_HZ: f64 = 866_300_000.0;
    const ACTUAL_RF_HZ: f64 = 866_300_000.488_759;
    const SAMPLE_RATE_HZ: f64 = 12_288_000.0;
    const TICK_RATE: u64 = 49_152_000;
    const SAMPLES: usize = 23_040;
    const START_TICK: u64 = 1_234_567_890_000;
    const ERROR_LIMIT: f32 = 1e-5;

    fn correction() -> TxFrequencyCorrection {
        let mut correction = TxFrequencyCorrection::software();
        correction
            .update_tune(CARRIER_HZ, ACTUAL_RF_HZ, 0.0)
            .unwrap();
        correction
    }

    fn at(ticks: u64) -> Schedule {
        Schedule::At(DeviceTime::from_ticks(ticks))
    }

    #[test]
    fn correction_defaults_to_software() {
        assert!(TxFrequencyCorrection::default().is_software());
    }

    #[test]
    fn hardware_mode_preserves_input_and_untimed_writes() {
        let mut correction = TxFrequencyCorrection::Hardware;
        let input = [Complex32::new(0.25, -0.5)];
        let result = correction
            .samples(&input, Schedule::Immediate, 0.0, 0)
            .unwrap();
        assert!(std::ptr::eq(result.as_ptr(), input.as_ptr()));
    }

    #[test]
    fn skipped_timed_intervals_and_restarts_preserve_absolute_phase() {
        const GAP_TICKS: u64 = TICK_RATE * 3;
        let mut correction = correction();
        let input = vec![Complex32::new(0.25, -0.5); SAMPLES];
        let mut max_error = 0.0_f32;
        for tick in [START_TICK, START_TICK + GAP_TICKS, START_TICK + TICK_RATE] {
            let output = correction
                .samples(&input, at(tick), SAMPLE_RATE_HZ, TICK_RATE)
                .unwrap();
            let shift_hz = CARRIER_HZ - ACTUAL_RF_HZ;
            for (n, (&sample, &actual)) in input.iter().zip(output).enumerate() {
                let time = tick as f64 / TICK_RATE as f64 + n as f64 / SAMPLE_RATE_HZ;
                let phase = std::f64::consts::TAU * shift_hz * time;
                let expected = sample * Complex32::new(phase.cos() as f32, phase.sin() as f32);
                max_error = max_error.max((actual - expected).norm());
            }
        }
        assert!(max_error < ERROR_LIMIT, "max_error={max_error}");
    }

    #[test]
    fn partial_write_retries_match_one_contiguous_timed_write() {
        const PREFIX_SAMPLES: usize = 2_201;
        const TICKS_PER_SAMPLE: u64 = 4;
        let input: Vec<_> = (0..SAMPLES)
            .map(|n| {
                let phase = std::f32::consts::TAU * n as f32 / SAMPLES as f32;
                Complex32::new(phase.cos(), phase.sin())
            })
            .collect();
        let mut whole = correction();
        let expected = whole
            .samples(&input, at(START_TICK), SAMPLE_RATE_HZ, TICK_RATE)
            .unwrap()
            .to_vec();
        let mut split = correction();
        let mut actual = split
            .samples(
                &input[..PREFIX_SAMPLES],
                at(START_TICK),
                SAMPLE_RATE_HZ,
                TICK_RATE,
            )
            .unwrap()
            .to_vec();
        actual.extend_from_slice(
            split
                .samples(
                    &input[PREFIX_SAMPLES..],
                    at(START_TICK + PREFIX_SAMPLES as u64 * TICKS_PER_SAMPLE),
                    SAMPLE_RATE_HZ,
                    TICK_RATE,
                )
                .unwrap(),
        );
        let max_error = expected
            .iter()
            .zip(actual)
            .map(|(a, b)| (*a - b).norm())
            .fold(0.0_f32, f32::max);
        assert!(max_error < ERROR_LIMIT, "max_error={max_error}");
    }

    #[test]
    fn software_mode_requires_tuning_timestamps_and_zero_hardware_shift() {
        let mut correction = TxFrequencyCorrection::software();
        let input = [Complex32::new(1.0, 0.0)];
        assert!(
            correction
                .samples(&input, at(START_TICK), SAMPLE_RATE_HZ, TICK_RATE)
                .is_err()
        );
        correction
            .update_tune(CARRIER_HZ, ACTUAL_RF_HZ, 0.0)
            .unwrap();
        assert!(
            correction
                .samples(&input, Schedule::Immediate, SAMPLE_RATE_HZ, TICK_RATE)
                .is_err()
        );
        assert!(correction.tune_request(CARRIER_HZ, 1.0).is_err());
        assert!(
            correction
                .update_tune(CARRIER_HZ, ACTUAL_RF_HZ, 0.5)
                .is_err()
        );
        assert!(
            correction
                .samples(&input, at(START_TICK), SAMPLE_RATE_HZ, TICK_RATE)
                .is_err()
        );
        assert!(correction.update_tune(CARRIER_HZ, f64::NAN, 0.0).is_err());
    }

    #[test]
    fn retuning_replaces_the_software_frequency_correction() {
        const RETUNED_RF_HZ: f64 = CARRIER_HZ - 0.25;
        let mut correction = correction();
        correction
            .update_tune(CARRIER_HZ, RETUNED_RF_HZ, 0.0)
            .unwrap();
        let input = [Complex32::new(1.0, 0.0)];
        let output = correction
            .samples(&input, at(TICK_RATE), SAMPLE_RATE_HZ, TICK_RATE)
            .unwrap();
        assert!((output[0] - Complex32::new(0.0, 1.0)).norm() < ERROR_LIMIT);
    }
}
