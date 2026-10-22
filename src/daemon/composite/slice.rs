//! Per-client rate conversion and frequency shifting against the shared device stream.

use num_complex::Complex32;

use super::resample::RationalResampler;
use super::{IntermediateStage, SlicePlan};
use crate::dsp::PhasorNco;
use crate::dsp::fir::{DecimatingComplexFir32, PolyphaseComplexFir32};

enum TxRateConverter {
    Integer(PolyphaseComplexFir32),
    Rational(RationalResampler),
    TwoStage {
        rational: RationalResampler,
        integer: PolyphaseComplexFir32,
        mid: Vec<Complex32>,
    },
}

enum RxRateConverter {
    Integer(DecimatingComplexFir32),
    Rational(RationalResampler),
    TwoStage {
        integer: DecimatingComplexFir32,
        rational: RationalResampler,
        factor: usize,
        mid: Vec<Complex32>,
    },
}

/// Intermediate and client rates reduced to lowest terms, as `(mid, client)`. The stage
/// factor divides the reduced slice ratio, so these stay reduced.
fn mid_client_terms(stage: &IntermediateStage, plan: &SlicePlan) -> (usize, usize) {
    (plan.ratio / stage.factor, plan.denominator)
}

fn client_rate_hz(plan: &SlicePlan, device_rate_hz: u64) -> f64 {
    device_rate_hz as f64 * plan.denominator as f64 / plan.ratio as f64
}

/// One client's contribution to the shared TX stream.
///
/// Every grid block must be accumulated, including one the client supplied nothing for.
/// Silence keeps the interpolator history and the oscillator phase locked to device time,
/// so a client that misses a block does not shift the samples that follow it.
///
/// A block holds a fractional number of client samples in general. The converter output
/// past the block end is held in `carry` and starts the next block.
pub(crate) struct TxSlice {
    converter: TxRateConverter,
    nco: PhasorNco,
    gain: f32,
    staged: Vec<Complex32>,
    carry: Vec<Complex32>,
}

impl TxSlice {
    pub(crate) fn new(plan: &SlicePlan, device_rate_hz: u64, gain: f32) -> Self {
        let client_hz = client_rate_hz(plan, device_rate_hz);
        Self {
            converter: match &plan.intermediate {
                _ if plan.denominator == 1 => TxRateConverter::Integer(
                    PolyphaseComplexFir32::with_interpolate(&plan.taps, plan.ratio),
                ),
                None => TxRateConverter::Rational(RationalResampler::new(
                    plan.ratio,
                    plan.denominator,
                    client_hz,
                    plan.passband_hz,
                )),
                Some(stage) => {
                    let (mid, client) = mid_client_terms(stage, plan);
                    TxRateConverter::TwoStage {
                        rational: RationalResampler::new(mid, client, client_hz, plan.passband_hz),
                        integer: PolyphaseComplexFir32::with_interpolate(&stage.taps, stage.factor),
                        mid: Vec::new(),
                    }
                }
            },
            nco: PhasorNco::from_offset_hz(plan.offset_hz, device_rate_hz as usize),
            gain,
            staged: Vec::new(),
            carry: Vec::new(),
        }
    }

    /// Starts the slice at device sample `device_sample` with client sample
    /// `first_input`, the first one whose time falls at or after it. Device samples before
    /// that client sample's first output are silence.
    pub(crate) fn align(&mut self, device_sample: u64, first_input: u64) {
        let first_output = match &mut self.converter {
            TxRateConverter::Integer(filter) => first_input * filter.interpolate() as u64,
            TxRateConverter::Rational(filter) => filter.start_at_input(first_input),
            TxRateConverter::TwoStage {
                rational, integer, ..
            } => rational.start_at_input(first_input) * integer.interpolate() as u64,
        };
        self.carry.clear();
        self.carry.resize(
            (first_output - device_sample) as usize,
            Complex32::default(),
        );
    }

    /// Adds this client's shifted contribution to `out`.
    ///
    /// `input` must be every client sample whose time falls inside the block, so
    /// the converter output always covers it.
    pub(crate) fn accumulate_into(&mut self, input: &[Complex32], out: &mut [Complex32]) {
        match &mut self.converter {
            TxRateConverter::Integer(filter) => filter.process_block_into(input, &mut self.staged),
            TxRateConverter::Rational(filter) => filter.process_into(input, &mut self.staged),
            TxRateConverter::TwoStage {
                rational,
                integer,
                mid,
            } => {
                rational.process_into(input, mid);
                integer.process_block_into(mid, &mut self.staged);
            }
        }
        let from_carry = self.carry.len().min(out.len());
        let (head, tail) = out.split_at_mut(from_carry);
        self.nco
            .rotate_accumulate(&self.carry[..from_carry], head, self.gain);
        let take = tail.len().min(self.staged.len());
        debug_assert_eq!(take, tail.len(), "TX slice output fell short of a block");
        let (filled, short) = tail.split_at_mut(take);
        self.nco
            .rotate_accumulate(&self.staged[..take], filled, self.gain);
        if !short.is_empty() {
            let silence = vec![Complex32::default(); short.len()];
            self.nco.rotate_accumulate(&silence, short, self.gain);
        }
        self.carry.drain(..from_carry);
        self.carry.extend_from_slice(&self.staged[take..]);
    }
}

/// One client's view of the shared RX stream.
pub(crate) struct RxSlice {
    nco: PhasorNco,
    converter: RxRateConverter,
    staged: Vec<Complex32>,
}

impl RxSlice {
    pub(crate) fn new(plan: &SlicePlan, device_rate_hz: u64) -> Self {
        Self {
            nco: PhasorNco::from_offset_hz(-plan.offset_hz, device_rate_hz as usize),
            converter: match &plan.intermediate {
                _ if plan.denominator == 1 => {
                    RxRateConverter::Integer(DecimatingComplexFir32::new(&plan.taps, plan.ratio))
                }
                None => RxRateConverter::Rational(RationalResampler::new(
                    plan.denominator,
                    plan.ratio,
                    device_rate_hz as f64,
                    plan.passband_hz,
                )),
                Some(stage) => {
                    let (mid, client) = mid_client_terms(stage, plan);
                    RxRateConverter::TwoStage {
                        integer: DecimatingComplexFir32::new(&stage.taps, stage.factor),
                        rational: RationalResampler::new(
                            client,
                            mid,
                            stage.rate_hz(device_rate_hz),
                            plan.passband_hz,
                        ),
                        factor: stage.factor,
                        mid: Vec::new(),
                    }
                }
            },
            staged: Vec::new(),
        }
    }

    /// Continues the output grid of a slice that has consumed `device_samples`, so a
    /// replacement slice emits at the same client sample times its predecessor would have.
    pub(crate) fn align(&mut self, device_samples: u64) {
        match &mut self.converter {
            RxRateConverter::Integer(filter) => filter.set_inputs_consumed(device_samples),
            RxRateConverter::Rational(filter) => {
                filter.start_at_input(device_samples);
            }
            RxRateConverter::TwoStage {
                integer,
                rational,
                factor,
                ..
            } => {
                integer.set_inputs_consumed(device_samples);
                rational.start_at_input(device_samples.div_ceil(*factor as u64));
            }
        }
    }

    /// Group delay this slice adds, in device samples.
    pub(crate) fn group_delay_samples(&self) -> usize {
        match &self.converter {
            RxRateConverter::Integer(filter) => filter.group_delay_samples(),
            RxRateConverter::Rational(filter) => filter.group_delay_samples(),
            RxRateConverter::TwoStage {
                integer,
                rational,
                factor,
                ..
            } => integer.group_delay_samples() + rational.group_delay_samples() * factor,
        }
    }

    /// Replaces `out` with this client's samples taken from one device block.
    pub(crate) fn process_into(&mut self, device_samples: &[Complex32], out: &mut Vec<Complex32>) {
        self.nco.rotate_into(device_samples, &mut self.staged);
        match &mut self.converter {
            RxRateConverter::Integer(filter) => filter.process_block_into(&self.staged, out),
            RxRateConverter::Rational(filter) => filter.process_into(&self.staged, out),
            RxRateConverter::TwoStage {
                integer,
                rational,
                mid,
                ..
            } => {
                integer.process_block_into(&self.staged, mid);
                rational.process_into(mid, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::composite::{CompositeConfig, Direction};

    const PLAN: CompositeConfig = CompositeConfig {
        device_rate_hz: 3_888_000,
        tx_center_hz: 867_520_000,
        rx_center_hz: 822_520_000,
        tx_digital_backoff_db: 0.0,
        block_duration_ms: 5.0 / 6.0,
        tx_deadline_ms: 2.0,
    };
    const CLIENT_RATE_HZ: u64 = 388_800;
    const RATIO: usize = 10;
    // Five milliseconds at the composite rate, a whole number of client samples.
    const BLOCK: usize = 19_440;

    fn slice_plan(direction: Direction, carrier_hz: u64) -> SlicePlan {
        PLAN.plan_slice(direction, CLIENT_RATE_HZ as f64, carrier_hz, Some(30_000))
            .unwrap()
    }

    /// Mean magnitude of the input's component at `offset_hz`, by coherent integration.
    fn tone_magnitude(samples: &[Complex32], offset_hz: i64, rate_hz: u64) -> f32 {
        let step = -std::f64::consts::TAU * offset_hz as f64 / rate_hz as f64;
        let mut sum = Complex32::default();
        for (n, &sample) in samples.iter().enumerate() {
            let phase = step * n as f64;
            sum += sample * Complex32::new(phase.cos() as f32, phase.sin() as f32);
        }
        sum.norm() / samples.len() as f32
    }

    fn dc(len: usize) -> Vec<Complex32> {
        vec![Complex32::new(1.0, 0.0); len]
    }

    #[test]
    fn half_rate_three_carrier_plan_recovers_each_slice() {
        const DEVICE_RATE: u64 = 4_915_200;
        const DEVICE_BLOCK: usize = 9_216;
        const BLOCKS: usize = 12;
        const AMPLITUDE: f32 = 0.1;
        let config = CompositeConfig {
            device_rate_hz: DEVICE_RATE,
            tx_center_hz: 869_700_000,
            rx_center_hz: 869_700_000,
            tx_digital_backoff_db: 0.0,
            block_duration_ms: 1.875,
            tx_deadline_ms: 5.0,
        };
        let carriers = [
            (648_000, 868_700_000, 200_000, 1_000),
            (388_800, 870_930_000, 30_000, 2_000),
            (4_915_200, 869_700_000, 1_500_000, 600_000),
        ];
        let plans = carriers
            .iter()
            .map(|&(rate, hz, bandwidth, _)| {
                config
                    .plan_slice(Direction::Tx, rate as f64, hz, Some(bandwidth))
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let mut transmitters = plans
            .iter()
            .map(|plan| TxSlice::new(plan, DEVICE_RATE, 1.0))
            .collect::<Vec<_>>();
        let mut receivers = plans
            .iter()
            .map(|plan| RxSlice::new(plan, DEVICE_RATE))
            .collect::<Vec<_>>();
        let mut output = vec![Complex32::default(); DEVICE_BLOCK];
        let mut recovered = Vec::new();
        for block in 0..BLOCKS {
            output.fill(Complex32::default());
            for ((tx, plan), &(rate, _, _, tone)) in
                transmitters.iter_mut().zip(&plans).zip(&carriers)
            {
                let count = DEVICE_BLOCK / plan.ratio * plan.denominator;
                let input = (0..count)
                    .map(|index| {
                        let phase =
                            std::f64::consts::TAU * tone as f64 * (block * count + index) as f64
                                / rate as f64;
                        Complex32::new(
                            AMPLITUDE * phase.cos() as f32,
                            AMPLITUDE * phase.sin() as f32,
                        )
                    })
                    .collect::<Vec<_>>();
                tx.accumulate_into(&input, &mut output);
            }
            for ((rx, plan), &(rate, _, _, tone)) in receivers.iter_mut().zip(&plans).zip(&carriers)
            {
                rx.process_into(&output, &mut recovered);
                assert_eq!(
                    recovered.len(),
                    DEVICE_BLOCK / plan.ratio * plan.denominator
                );
                if block == BLOCKS - 1 {
                    let amplitude = tone_magnitude(&recovered, tone, rate);
                    assert!(
                        (amplitude - AMPLITUDE).abs() < 0.001,
                        "rate={rate} amplitude={amplitude}"
                    );
                }
            }
        }
    }

    #[test]
    fn three_carriers_preserve_uplink_and_downlink() {
        const DEVICE_RATE: u64 = 9_720_000;
        const DEVICE_BLOCK: usize = 8_100;
        let config = CompositeConfig {
            device_rate_hz: DEVICE_RATE,
            tx_center_hz: 865_200_000,
            rx_center_hz: 820_200_000,
            tx_digital_backoff_db: 0.0,
            block_duration_ms: 5.0 / 6.0,
            tx_deadline_ms: 2.0,
        };
        let carriers = [
            (648_000, 860_850_000, 200_000, 1_000),
            (388_800, 869_040_000, 30_000, 2_000),
            (4_915_200, 864_570_000, 1_500_000, 600_000),
        ];
        let plans = carriers
            .iter()
            .map(|&(rate, hz, bw, _)| {
                config
                    .plan_slice(Direction::Tx, rate as f64, hz, Some(bw))
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            super::super::grid_block_samples(DEVICE_RATE).unwrap(),
            DEVICE_BLOCK
        );
        let mut transmitters = plans
            .iter()
            .map(|p| TxSlice::new(p, DEVICE_RATE, 1.0))
            .collect::<Vec<_>>();
        let rx_plan = config
            .plan_slice(Direction::Rx, 4_915_200.0, 819_570_000, Some(1_500_000))
            .unwrap();
        let mut receiver = RxSlice::new(&rx_plan, DEVICE_RATE);
        let mut composite = vec![Complex32::default(); DEVICE_BLOCK];
        let mut recovered = Vec::new();
        let mut max_tx = std::time::Duration::ZERO;
        let mut max_rx = std::time::Duration::ZERO;
        for block in 0..8 {
            composite.fill(Complex32::default());
            let inputs = carriers
                .iter()
                .map(|&(rate, _, _, tone)| {
                    let count = rate as usize / 1200;
                    (0..count)
                        .map(|n| {
                            let phase =
                                std::f64::consts::TAU * tone as f64 * (block * count + n) as f64
                                    / rate as f64;
                            Complex32::new(0.1 * phase.cos() as f32, 0.1 * phase.sin() as f32)
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            let start = std::time::Instant::now();
            for (tx, input) in transmitters.iter_mut().zip(inputs) {
                tx.accumulate_into(&input, &mut composite);
            }
            max_tx = max_tx.max(start.elapsed());
            let start = std::time::Instant::now();
            receiver.process_into(&composite, &mut recovered);
            max_rx = max_rx.max(start.elapsed());
            assert_eq!(recovered.len(), 4_096);
        }
        let amplitude = tone_magnitude(&recovered, 600_000, 4_915_200);
        assert!(
            (amplitude - 0.1).abs() < 0.001,
            "recovered amplitude={amplitude}"
        );
        eprintln!(
            "three carriers per 0.833ms block: max_tx_us={} max_rx_us={} amplitude={amplitude}",
            max_tx.as_micros(),
            max_rx.as_micros()
        );
    }

    #[test]
    fn a_wide_client_plan_rejects_the_mirror_and_interpolation_image() {
        const DEVICE_RATE: u64 = 9_720_000;
        const WIDE_RATE: u64 = 648_000;
        const CENTER_HZ: u64 = 864_945_000;
        const CARRIER_HZ: u64 = 860_850_000;
        const TEST_TONE_HZ: i64 = 1_000;
        const BLOCKS: usize = 8;
        let config = CompositeConfig {
            device_rate_hz: DEVICE_RATE,
            tx_center_hz: CENTER_HZ,
            rx_center_hz: CENTER_HZ,
            tx_digital_backoff_db: -3.0,
            block_duration_ms: 5.0 / 6.0,
            tx_deadline_ms: 2.0,
        };
        let plan = config
            .plan_slice(Direction::Tx, WIDE_RATE as f64, CARRIER_HZ, Some(200_000))
            .unwrap();
        let mut tx = TxSlice::new(&plan, DEVICE_RATE, config.tx_scale());
        let block_samples = DEVICE_RATE as usize / 200;
        let client_samples = block_samples / plan.ratio;
        let mut output = vec![Complex32::default(); block_samples];
        let mut steady = Vec::new();
        for block in 0..BLOCKS {
            let input = (0..client_samples)
                .map(|index| {
                    let sample = block * client_samples + index;
                    let angle = std::f64::consts::TAU * TEST_TONE_HZ as f64 * sample as f64
                        / WIDE_RATE as f64;
                    Complex32::new(angle.cos() as f32, angle.sin() as f32)
                })
                .collect::<Vec<_>>();
            output.fill(Complex32::default());
            tx.accumulate_into(&input, &mut output);
            if block != 0 {
                steady.extend_from_slice(&output);
            }
        }
        let wanted_hz = plan.offset_hz + TEST_TONE_HZ;
        let wanted = tone_magnitude(&steady, wanted_hz, DEVICE_RATE);
        let mirror = tone_magnitude(&steady, -wanted_hz, DEVICE_RATE);
        let image = tone_magnitude(&steady, wanted_hz + WIDE_RATE as i64, DEVICE_RATE);
        let mirror_db = 20.0 * (mirror / wanted).max(1e-10).log10();
        let image_db = 20.0 * (image / wanted).max(1e-10).log10();
        eprintln!(
            "actual composite plan: wanted_amplitude={wanted:.6} mirror_dbc={mirror_db:.1} interpolation_image_dbc={image_db:.1}"
        );
        assert!(wanted > 0.65 && wanted < 0.75, "wanted amplitude {wanted}");
        assert!(mirror_db < -60.0, "mirror {mirror_db} dBc");
        assert!(image_db < -60.0, "interpolation image {image_db} dBc");
    }

    #[test]
    fn a_tx_slice_lands_its_client_at_the_configured_offset() {
        let plan = slice_plan(Direction::Tx, 869_040_000);
        assert_eq!(plan.offset_hz, 1_520_000);
        let mut slice = TxSlice::new(&plan, PLAN.device_rate_hz, 1.0);

        // Run several blocks so the interpolator reaches steady state before measuring.
        let mut out = vec![Complex32::default(); BLOCK];
        for _ in 0..4 {
            out.iter_mut().for_each(|s| *s = Complex32::default());
            slice.accumulate_into(&dc(BLOCK / RATIO), &mut out);
        }

        let at_carrier = tone_magnitude(&out, 1_520_000, PLAN.device_rate_hz);
        let at_center = tone_magnitude(&out, 0, PLAN.device_rate_hz);
        assert!(at_carrier > 0.95, "carrier only {at_carrier}");
        assert!(at_center < 0.01, "leaked {at_center} at center");
    }

    #[test]
    fn two_tx_slices_sum_without_landing_on_each_other() {
        let low = slice_plan(Direction::Tx, 866_000_000);
        let high = slice_plan(Direction::Tx, 869_040_000);
        let mut low_slice = TxSlice::new(&low, PLAN.device_rate_hz, 0.5);
        let mut high_slice = TxSlice::new(&high, PLAN.device_rate_hz, 0.25);

        let mut out = vec![Complex32::default(); BLOCK];
        for _ in 0..4 {
            out.iter_mut().for_each(|s| *s = Complex32::default());
            low_slice.accumulate_into(&dc(BLOCK / RATIO), &mut out);
            high_slice.accumulate_into(&dc(BLOCK / RATIO), &mut out);
        }

        let low = tone_magnitude(&out, -1_520_000, PLAN.device_rate_hz);
        let high = tone_magnitude(&out, 1_520_000, PLAN.device_rate_hz);
        assert!((low - 0.5).abs() < 0.02, "low slice at {low}");
        assert!((high - 0.25).abs() < 0.02, "high slice at {high}");
    }

    #[test]
    fn an_rx_slice_recovers_only_its_own_carrier() {
        let plan = slice_plan(Direction::Rx, 824_040_000);
        assert_eq!(plan.offset_hz, 1_520_000);
        let mut slice = RxSlice::new(&plan, PLAN.device_rate_hz);

        // A composite of both uplink carriers, the wanted one at half amplitude.
        let device: Vec<Complex32> = (0..BLOCK * 2)
            .map(|n| {
                let wanted = std::f64::consts::TAU * 1_520_000.0 * n as f64 / 3_888_000.0;
                let other = std::f64::consts::TAU * -1_520_000.0 * n as f64 / 3_888_000.0;
                Complex32::new(0.5 * wanted.cos() as f32, 0.5 * wanted.sin() as f32)
                    + Complex32::new(other.cos() as f32, other.sin() as f32)
            })
            .collect();

        let mut out = Vec::new();
        slice.process_into(&device, &mut out);
        assert_eq!(out.len(), device.len() / RATIO);

        // Skip the filter transient, then the wanted carrier sits at baseband.
        let steady = &out[slice.group_delay_samples() / RATIO + 32..];
        let baseband = tone_magnitude(steady, 0, CLIENT_RATE_HZ);
        assert!((baseband - 0.5).abs() < 0.02, "recovered {baseband}");
    }

    #[test]
    fn an_rx_slice_keeps_its_decimation_phase_across_blocks() {
        let plan = slice_plan(Direction::Rx, 821_000_000);
        let device: Vec<Complex32> = (0..BLOCK)
            .map(|n| Complex32::new(n as f32 * 1e-4, -(n as f32) * 1e-4))
            .collect();

        let mut whole = RxSlice::new(&plan, PLAN.device_rate_hz);
        let mut split = RxSlice::new(&plan, PLAN.device_rate_hz);
        let mut all = Vec::new();
        whole.process_into(&device, &mut all);

        let mut pieces = Vec::new();
        let mut part = Vec::new();
        for chunk in device.chunks(RATIO * 37) {
            split.process_into(chunk, &mut part);
            pieces.extend_from_slice(&part);
        }
        assert_eq!(all, pieces);
    }

    #[test]
    fn a_slice_at_the_device_rate_passes_samples_through() {
        let plan = PLAN
            .plan_slice(
                Direction::Tx,
                PLAN.device_rate_hz as f64,
                PLAN.tx_center_hz,
                None,
            )
            .unwrap();
        let mut slice = TxSlice::new(&plan, PLAN.device_rate_hz, 1.0);
        let input: Vec<Complex32> = (0..16)
            .map(|n| Complex32::new(n as f32, -(n as f32)))
            .collect();
        let mut out = vec![Complex32::default(); 16];
        slice.accumulate_into(&input, &mut out);
        assert_eq!(out, input);
    }

    /// A band-limited signal of several tones, distinct per client, standing in for a
    /// modulated waveform. Content stays well inside the 15 kHz slice passband.
    fn waveform(len: usize, seed: usize) -> Vec<Complex32> {
        let tones = [
            (1_300.0 + seed as f64 * 190.0, 0.5, 0.0),
            (-4_100.0 + seed as f64 * 70.0, 0.3, 1.1),
            (7_200.0 - seed as f64 * 250.0, 0.2, 2.4),
        ];
        (0..len)
            .map(|n| {
                let t = n as f64 / CLIENT_RATE_HZ as f64;
                tones
                    .iter()
                    .fold(Complex32::default(), |sum, &(hz, amp, ph)| {
                        let phase = std::f64::consts::TAU * hz * t + ph;
                        sum + Complex32::new((amp * phase.cos()) as f32, (amp * phase.sin()) as f32)
                    })
            })
            .collect()
    }

    /// Error energy relative to signal energy, after aligning `got` to `want` at the
    /// delay that fits best. Returns the residual and the delay it found.
    fn residual(want: &[Complex32], got: &[Complex32], max_delay: usize) -> (f32, usize) {
        let mut best = (f32::INFINITY, 0usize);
        for delay in 0..=max_delay {
            if delay + 64 >= got.len() {
                break;
            }
            let span = (want.len() - 64).min(got.len() - delay - 64);
            let (mut err, mut signal) = (0.0f32, 0.0f32);
            for index in 32..span {
                let difference = got[delay + index] - want[index];
                err += difference.norm_sqr();
                signal += want[index].norm_sqr();
            }
            if signal > 0.0 {
                let ratio = err / signal;
                if ratio < best.0 {
                    best = (ratio, delay);
                }
            }
        }
        best
    }

    #[test]
    fn a_modulated_signal_survives_the_round_trip_through_a_summed_stream() {
        let tx_plans = [
            slice_plan(Direction::Tx, 866_000_000),
            slice_plan(Direction::Tx, 869_040_000),
        ];
        // The receive slices use the same offsets, since both links sit the same
        // distance either side of their own center.
        let rx_plans = [
            slice_plan(Direction::Rx, 821_000_000),
            slice_plan(Direction::Rx, 824_040_000),
        ];
        let mut tx: Vec<TxSlice> = tx_plans
            .iter()
            .map(|plan| TxSlice::new(plan, PLAN.device_rate_hz, 1.0))
            .collect();
        let mut rx: Vec<RxSlice> = rx_plans
            .iter()
            .map(|plan| RxSlice::new(plan, PLAN.device_rate_hz))
            .collect();

        let blocks = 6;
        let per_block = BLOCK / RATIO;
        let sent: Vec<Vec<Complex32>> = (0..2)
            .map(|seed| waveform(per_block * blocks, seed))
            .collect();
        let mut received: Vec<Vec<Complex32>> = vec![Vec::new(), Vec::new()];
        let mut composite = vec![Complex32::default(); BLOCK];
        let mut sliced = Vec::new();

        for block in 0..blocks {
            // Both clients are summed into one device block, as the grid does.
            composite.iter_mut().for_each(|s| *s = Complex32::default());
            let span = block * per_block..(block + 1) * per_block;
            for (index, slice) in tx.iter_mut().enumerate() {
                slice.accumulate_into(&sent[index][span.clone()], &mut composite);
            }
            // Each client then takes its own carrier back out of that same block.
            for (index, slice) in rx.iter_mut().enumerate() {
                slice.process_into(&composite, &mut sliced);
                received[index].extend_from_slice(&sliced);
            }
        }

        for index in 0..2 {
            assert_eq!(received[index].len(), per_block * blocks);
            let (error, delay) = residual(&sent[index], &received[index], 64);
            // The two filters' group delay shows up as a whole-sample shift, and what
            // is left is filter ripple rather than the peer's signal.
            assert!(
                error < 0.02,
                "client {index} recovered with {:.1}% error at delay {delay}",
                error * 100.0
            );
            assert!(delay > 0, "client {index} showed no filter delay at all");
        }
    }

    #[test]
    fn a_peer_at_the_other_carrier_does_not_bleed_into_a_recovered_signal() {
        // One client transmits, the other is silent, and the silent client's receiver
        // must read close to nothing rather than its peer's waveform.
        let tx_plan = slice_plan(Direction::Tx, 869_040_000);
        let quiet_rx = slice_plan(Direction::Rx, 821_000_000);
        let mut tx = TxSlice::new(&tx_plan, PLAN.device_rate_hz, 1.0);
        let mut rx = RxSlice::new(&quiet_rx, PLAN.device_rate_hz);

        let per_block = BLOCK / RATIO;
        let signal = waveform(per_block * 4, 0);
        let mut composite = vec![Complex32::default(); BLOCK];
        let mut sliced = Vec::new();
        let mut leaked = Vec::new();
        for block in 0..4 {
            composite.iter_mut().for_each(|s| *s = Complex32::default());
            tx.accumulate_into(
                &signal[block * per_block..(block + 1) * per_block],
                &mut composite,
            );
            rx.process_into(&composite, &mut sliced);
            leaked.extend_from_slice(&sliced);
        }

        let sent_power: f32 =
            signal.iter().map(|s| s.norm_sqr()).sum::<f32>() / signal.len() as f32;
        let steady = &leaked[leaked.len() / 2..];
        let leaked_power: f32 =
            steady.iter().map(|s| s.norm_sqr()).sum::<f32>() / steady.len() as f32;
        let rejection_db = 10.0 * (leaked_power / sent_power).max(1e-20).log10();
        assert!(
            rejection_db < -45.0,
            "the peer carrier leaked at only {rejection_db:.1} dB"
        );
    }

    const NARROW_PLAN: CompositeConfig = CompositeConfig {
        device_rate_hz: 12_288_000,
        tx_center_hz: 866_300_000,
        rx_center_hz: 821_300_000,
        tx_digital_backoff_db: 0.0,
        block_duration_ms: 1.875,
        tx_deadline_ms: 5.0,
    };
    const NARROW_BLOCK: usize = 23_040;
    /// `(rate, TX carrier, bandwidth, test tone)`.
    const NARROW_CLIENTS: [(u64, u64, u64, i64); 3] = [
        (4_915_200, 869_730_000, 1_250_000, 300_000),
        (800_000, 860_850_000, 200_000, 20_000),
        (388_800, 869_040_000, 30_000, 5_000),
    ];
    const DUPLEX_HZ: u64 = 45_000_000;

    fn tone(
        rate_hz: u64,
        frequency_hz: f64,
        start: usize,
        len: usize,
        amplitude: f32,
    ) -> Vec<Complex32> {
        (start..start + len)
            .map(|n| {
                let phase = std::f64::consts::TAU * frequency_hz * n as f64 / rate_hz as f64;
                Complex32::new(
                    amplitude * phase.cos() as f32,
                    amplitude * phase.sin() as f32,
                )
            })
            .collect()
    }

    /// Tone amplitude and residual power. Fits in `f64` so a deep residual survives.
    fn fit_tone(samples: &[Complex32], frequency_hz: f64, rate_hz: u64) -> (f64, f64) {
        let step = std::f64::consts::TAU * frequency_hz / rate_hz as f64;
        let basis = |n: usize| num_complex::Complex64::from_polar(1.0, step * n as f64);
        let len = samples.len() as f64;
        let amplitude = samples
            .iter()
            .enumerate()
            .map(|(n, s)| num_complex::Complex64::new(s.re as f64, s.im as f64) * basis(n).conj())
            .sum::<num_complex::Complex64>()
            / len;
        let residual = samples
            .iter()
            .enumerate()
            .map(|(n, s)| {
                (num_complex::Complex64::new(s.re as f64, s.im as f64) - amplitude * basis(n))
                    .norm_sqr()
            })
            .sum::<f64>()
            / len;
        (amplitude.norm(), residual)
    }

    const NARROW_BLOCKS: usize = 12;

    fn narrow_tx_leakage_db(plan: &SlicePlan, rate: u64, tone_hz: i64) -> (f64, f64) {
        let mut slice = TxSlice::new(plan, NARROW_PLAN.device_rate_hz, 1.0);
        let per_block = NARROW_BLOCK / plan.ratio * plan.denominator;
        let mut out = vec![Complex32::default(); NARROW_BLOCK];
        for block in 0..NARROW_BLOCKS {
            out.fill(Complex32::default());
            let input = tone(rate, tone_hz as f64, block * per_block, per_block, 0.5);
            slice.accumulate_into(&input, &mut out);
        }
        let (amplitude, residual) = fit_tone(
            &out,
            (plan.offset_hz + tone_hz) as f64,
            NARROW_PLAN.device_rate_hz,
        );
        (
            amplitude,
            10.0 * (residual / (amplitude * amplitude)).log10(),
        )
    }

    fn narrow_rx_residual_db(
        plan: &SlicePlan,
        rate: u64,
        tone_hz: i64,
        interferers: &[f64],
    ) -> (f64, f64) {
        let device = NARROW_PLAN.device_rate_hz;
        let offset = plan.offset_hz as f64;
        let mut rx = RxSlice::new(plan, device);
        let mut recovered = Vec::new();
        for block in 0..NARROW_BLOCKS {
            let start = block * NARROW_BLOCK;
            let mut input = tone(device, offset + tone_hz as f64, start, NARROW_BLOCK, 0.1);
            for &interferer in interferers {
                let unwanted = tone(device, offset + interferer, start, NARROW_BLOCK, 1.0);
                for (slot, sample) in input.iter_mut().zip(unwanted) {
                    *slot += sample;
                }
            }
            rx.process_into(&input, &mut recovered);
        }
        let (amplitude, residual) = fit_tone(&recovered, tone_hz as f64, rate);
        (amplitude, 10.0 * residual.log10())
    }

    fn alias_interferers(rate: u64, mid: Option<f64>, tone_hz: i64) -> Vec<f64> {
        let mut interferers = vec![0.6 * rate as f64, -0.75 * rate as f64];
        if let Some(mid) = mid {
            interferers.extend([mid - tone_hz as f64, -(mid + tone_hz as f64)]);
        }
        interferers
    }

    #[test]
    fn narrow_tx_slices_leave_everything_outside_their_tone_60_db_down() {
        for (rate, carrier, bandwidth, tone_hz) in NARROW_CLIENTS {
            let plan = NARROW_PLAN
                .plan_slice(Direction::Tx, rate as f64, carrier, Some(bandwidth))
                .unwrap();
            if rate < 1_000_000 {
                assert!(
                    plan.intermediate.is_some(),
                    "{rate} Hz TX stayed single-stage"
                );
            }
            let (amplitude, leakage_db) = narrow_tx_leakage_db(&plan, rate, tone_hz);
            assert!(
                (amplitude - 0.5).abs() < 0.005,
                "{rate} Hz tone came out at {amplitude}"
            );
            assert!(leakage_db < -60.0, "{rate} Hz TX leaks {leakage_db:.1} dB");
        }
    }

    #[test]
    fn narrow_rx_slices_reject_interferers_that_would_alias() {
        let device = NARROW_PLAN.device_rate_hz;
        for (rate, tx_carrier, bandwidth, tone_hz) in NARROW_CLIENTS {
            let plan = NARROW_PLAN
                .plan_slice(
                    Direction::Rx,
                    rate as f64,
                    tx_carrier - DUPLEX_HZ,
                    Some(bandwidth),
                )
                .unwrap();
            let mid = plan
                .intermediate
                .as_ref()
                .map(|stage| stage.rate_hz(device));
            if rate < 1_000_000 {
                assert!(mid.is_some(), "{rate} Hz RX stayed single-stage");
            }
            let interferers = alias_interferers(rate, mid, tone_hz);
            let (amplitude, residual_db) =
                narrow_rx_residual_db(&plan, rate, tone_hz, &interferers);
            assert!(
                (amplitude - 0.1).abs() < 0.001,
                "{rate} Hz tone came out at {amplitude}"
            );
            assert!(
                residual_db < -60.0,
                "{rate} Hz RX let {residual_db:.1} dB through"
            );
        }
    }
}
