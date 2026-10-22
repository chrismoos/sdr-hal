//! Per-client views of one shared RX stream.

use num_complex::Complex32;

use super::{RxSlice, SlicePlan};
use crate::error::Error;

/// One client's samples taken from a device block.
pub(crate) struct RxSliceBlock<'a> {
    /// Device tick of `samples[0]`, already corrected for the slice's group delay.
    pub(crate) time_ticks: u64,
    /// Samples at that client's own rate.
    pub(crate) samples: &'a [Complex32],
}

struct Slot {
    slice: RxSlice,
    ratio: usize,
    denominator: usize,
    /// Device tick the slice's output count is measured from. It moves by the size of
    /// any jump in device time, since the slice filters across a jump as if none occurred.
    origin_ticks: Option<u64>,
    /// Device tick the next block should start at if the stream is contiguous.
    next_block_ticks: Option<u64>,
    /// Device samples the slot has filtered since it was added.
    consumed_samples: u64,
    out: Vec<Complex32>,
    delivered_samples: u64,
    /// False once the client holding this slot went away.
    live: bool,
}

/// Hands every attached client its own carrier from the shared RX stream.
pub(crate) struct RxFanout {
    ticks_per_device_sample: u64,
    slots: Vec<Slot>,
}

impl RxFanout {
    /// Creates an empty fan-out.
    ///
    /// Returns an error unless `tick_rate` is a whole multiple of `device_rate_hz`, which
    /// keeps the shared device timeline exact. Client sample times are rounded to ticks.
    pub(crate) fn new(device_rate_hz: u64, tick_rate: u64) -> Result<Self, Error> {
        if device_rate_hz == 0 {
            return Err("a composite fan-out needs a positive device rate".into());
        }
        if !tick_rate.is_multiple_of(device_rate_hz) {
            return Err(format!(
                "composite mode needs a device clock that is a whole multiple of the device rate, and {tick_rate} Hz is not a multiple of {device_rate_hz} Hz"
            )
            .into());
        }
        Ok(Self {
            ticks_per_device_sample: tick_rate / device_rate_hz,
            slots: Vec::new(),
        })
    }

    /// Adds a client and returns its slot index, reusing a retired slot of the same rate
    /// ratio so a client that comes and goes cannot grow the fan-out without bound.
    pub(crate) fn add_slot(&mut self, plan: &SlicePlan, device_rate_hz: u64) -> usize {
        if let Some(index) = self.slots.iter().position(|slot| {
            !slot.live && slot.ratio == plan.ratio && slot.denominator == plan.denominator
        }) {
            let slot = &mut self.slots[index];
            slot.slice = RxSlice::new(plan, device_rate_hz);
            slot.origin_ticks = None;
            slot.next_block_ticks = None;
            slot.consumed_samples = 0;
            slot.delivered_samples = 0;
            slot.live = true;
            return index;
        }
        self.slots.push(Slot {
            slice: RxSlice::new(plan, device_rate_hz),
            ratio: plan.ratio,
            denominator: plan.denominator,
            origin_ticks: None,
            next_block_ticks: None,
            consumed_samples: 0,
            out: Vec::new(),
            delivered_samples: 0,
            live: true,
        });
        self.slots.len() - 1
    }

    /// Releases a slot whose client went away. The slot keeps its place so its peers'
    /// indices stay valid.
    pub(crate) fn retire_slot(&mut self, slot_index: usize) -> bool {
        let Some(slot) = self.slots.get_mut(slot_index) else {
            return false;
        };
        if !slot.live {
            return false;
        }
        slot.live = false;
        slot.out = Vec::new();
        true
    }

    /// Replaces one slot's slice, which is how a client retunes its carrier. The rate
    /// ratio cannot change, because the slot's sample times follow it.
    pub(crate) fn retune_slot(
        &mut self,
        slot_index: usize,
        plan: &SlicePlan,
        device_rate_hz: u64,
    ) -> Result<(), Error> {
        let slot = self.slots.get_mut(slot_index).ok_or_else(|| -> Error {
            format!("composite RX slot {slot_index} is not open").into()
        })?;
        if plan.ratio != slot.ratio || plan.denominator != slot.denominator {
            return Err(format!(
                "composite RX slot {slot_index} runs at ratio {}, so it cannot retune to ratio {}",
                slot.ratio, plan.ratio
            )
            .into());
        }
        // The client's sample times continue across a retune, so the new slice picks up
        // the old one's output grid rather than restarting it at this block.
        slot.slice = RxSlice::new(plan, device_rate_hz);
        slot.slice.align(slot.consumed_samples);
        Ok(())
    }

    pub(crate) fn slots(&self) -> usize {
        self.slots.len()
    }

    /// Slices one client's carrier out of a device block.
    ///
    /// The returned tick is the device time the first output sample represents, which is
    /// the block's own time less the slice filter's group delay. The first delay's worth
    /// of samples after RX starts are the filter transient.
    ///
    pub(crate) fn process(
        &mut self,
        slot_index: usize,
        device_samples: &[Complex32],
        device_ticks: u64,
    ) -> RxSliceBlock<'_> {
        let ticks_per_device_sample = self.ticks_per_device_sample;
        let slot = &mut self.slots[slot_index];
        let origin = slot.origin_ticks.get_or_insert(device_ticks);
        if let Some(expected) = slot.next_block_ticks
            && device_ticks != expected
        {
            *origin = origin.wrapping_add(device_ticks.wrapping_sub(expected));
        }
        let origin_ticks = *origin;
        slot.next_block_ticks = Some(
            device_ticks.saturating_add(device_samples.len() as u64 * ticks_per_device_sample),
        );
        slot.consumed_samples += device_samples.len() as u64;
        // A fresh slice's output n sits at exactly n * ratio / denominator device samples.
        let offset_ticks = ((slot.delivered_samples as u128
            * slot.ratio as u128
            * ticks_per_device_sample as u128
            + slot.denominator as u128 / 2)
            / slot.denominator as u128) as u64;
        let mut out = std::mem::take(&mut slot.out);
        slot.slice.process_into(device_samples, &mut out);
        slot.out = out;
        slot.delivered_samples += slot.out.len() as u64;
        let delay_ticks = slot.slice.group_delay_samples() as u64 * ticks_per_device_sample;
        RxSliceBlock {
            time_ticks: origin_ticks
                .saturating_add(offset_ticks)
                .saturating_sub(delay_ticks),
            samples: &slot.out,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::composite::{CompositeConfig, Direction, grid_block_samples};

    const PLAN: CompositeConfig = CompositeConfig {
        device_rate_hz: 3_888_000,
        tx_center_hz: 867_520_000,
        rx_center_hz: 822_520_000,
        tx_digital_backoff_db: 0.0,
        block_duration_ms: 5.0 / 6.0,
        tx_deadline_ms: 2.0,
    };
    const TICK_RATE: u64 = 31_104_000;
    const CLIENT_RATE_HZ: u64 = 388_800;
    const RATIO: usize = 10;

    fn plan_for(carrier_hz: u64) -> SlicePlan {
        PLAN.plan_slice(
            Direction::Rx,
            CLIENT_RATE_HZ as f64,
            carrier_hz,
            Some(30_000),
        )
        .unwrap()
    }

    /// Both uplink carriers at distinct amplitudes, as the device would deliver them.
    fn composite(len: usize, low_gain: f32, high_gain: f32) -> Vec<Complex32> {
        (0..len)
            .map(|n| {
                let low = std::f64::consts::TAU * -1_520_000.0 * n as f64 / 3_888_000.0;
                let high = std::f64::consts::TAU * 1_520_000.0 * n as f64 / 3_888_000.0;
                Complex32::new(low_gain * low.cos() as f32, low_gain * low.sin() as f32)
                    + Complex32::new(high_gain * high.cos() as f32, high_gain * high.sin() as f32)
            })
            .collect()
    }

    fn baseband_magnitude(samples: &[Complex32]) -> f32 {
        let sum: Complex32 = samples.iter().sum();
        sum.norm() / samples.len() as f32
    }

    #[test]
    fn rational_short_reads_keep_exact_sample_times() {
        const RATE: u64 = 9_720_000;
        let config = CompositeConfig {
            device_rate_hz: RATE,
            tx_center_hz: 865_200_000,
            rx_center_hz: 820_200_000,
            tx_digital_backoff_db: -3.0,
            block_duration_ms: 5.0 / 6.0,
            tx_deadline_ms: 2.0,
        };
        let plan = config
            .plan_slice(Direction::Rx, 4_915_200.0, 819_570_000, Some(1_500_000))
            .unwrap();
        let delay = RxSlice::new(&plan, RATE).group_delay_samples() as u64;
        let mut fanout = RxFanout::new(RATE, RATE).unwrap();
        let slot = fanout.add_slot(&plan, RATE);
        let input = vec![Complex32::new(1.0, 0.0); 48_600];
        let mut consumed = 0;
        let mut produced = 0;
        for chunk in input.chunks(2705) {
            let output = fanout.process(slot, chunk, RATE + consumed);
            if !output.samples.is_empty() {
                let expected = RATE + (produced * 2025 + 512) / 1024 - delay;
                assert_eq!(output.time_ticks, expected);
            }
            consumed += chunk.len() as u64;
            produced += output.samples.len() as u64;
        }
        assert_eq!(produced, 24_576);
    }

    #[test]
    fn short_reads_preserve_samples_and_output_timestamps() {
        for ratio in [10, 25] {
            let client_rate = PLAN.device_rate_hz / ratio;
            let plan = PLAN
                .plan_slice(Direction::Rx, client_rate as f64, 821_000_000, Some(30_000))
                .unwrap();
            let mut whole = RxFanout::new(PLAN.device_rate_hz, TICK_RATE).unwrap();
            let mut split = RxFanout::new(PLAN.device_rate_hz, TICK_RATE).unwrap();
            let whole_slot = whole.add_slot(&plan, PLAN.device_rate_hz);
            let split_slot = split.add_slot(&plan, PLAN.device_rate_hz);
            let device = composite(19_440, 0.8, 0.3);
            let start_ticks = TICK_RATE;
            let expected = whole.process(whole_slot, &device, start_ticks);
            let expected_ticks = expected.time_ticks;
            let expected_samples = expected.samples.to_vec();
            let mut actual = Vec::new();
            let mut offset = 0;
            for length in [2_705, 1, 24, 7, 8_003, 8_700] {
                let end = (offset + length).min(device.len());
                let output = split.process(
                    split_slot,
                    &device[offset..end],
                    start_ticks + offset as u64 * (TICK_RATE / PLAN.device_rate_hz),
                );
                if !output.samples.is_empty() {
                    assert_eq!(
                        output.time_ticks,
                        expected_ticks + actual.len() as u64 * (TICK_RATE / client_rate)
                    );
                }
                actual.extend_from_slice(output.samples);
                offset = end;
            }
            assert_eq!(offset, device.len());
            assert_eq!(actual, expected_samples);
        }
    }

    #[test]
    fn blocks_that_split_client_samples_keep_timestamps_contiguous() {
        const SPLIT_BLOCK: usize = 3_888;
        let mut fanout = RxFanout::new(PLAN.device_rate_hz, TICK_RATE).unwrap();
        let slot = fanout.add_slot(&plan_for(821_000_000), PLAN.device_rate_hz);
        let ticks_per_device_sample = TICK_RATE / PLAN.device_rate_hz;
        let ticks_per_client_sample = ticks_per_device_sample * RATIO as u64;
        let device = vec![Complex32::new(1.0, 0.0); SPLIT_BLOCK];
        let mut expected = None;
        for block in 0..10u64 {
            let ticks = TICK_RATE + block * SPLIT_BLOCK as u64 * ticks_per_device_sample;
            let sliced = fanout.process(slot, &device, ticks);
            if let Some(expected) = expected {
                assert_eq!(sliced.time_ticks, expected, "block {block}");
            }
            expected =
                Some(sliced.time_ticks + sliced.samples.len() as u64 * ticks_per_client_sample);
        }
    }

    #[test]
    fn a_jump_in_device_time_moves_the_client_timestamps_with_it() {
        const SPLIT_BLOCK: usize = 3_888;
        const JUMP_TICKS: u64 = 12_345;
        let mut fanout = RxFanout::new(PLAN.device_rate_hz, TICK_RATE).unwrap();
        let slot = fanout.add_slot(&plan_for(821_000_000), PLAN.device_rate_hz);
        let ticks_per_device_sample = TICK_RATE / PLAN.device_rate_hz;
        let ticks_per_client_sample = ticks_per_device_sample * RATIO as u64;
        let block_ticks = SPLIT_BLOCK as u64 * ticks_per_device_sample;
        let device = vec![Complex32::new(1.0, 0.0); SPLIT_BLOCK];
        let first = fanout.process(slot, &device, TICK_RATE);
        let contiguous = first.time_ticks + first.samples.len() as u64 * ticks_per_client_sample;
        let after_jump = fanout.process(slot, &device, TICK_RATE + block_ticks + JUMP_TICKS);
        assert_eq!(after_jump.time_ticks, contiguous + JUMP_TICKS);
    }

    #[test]
    fn a_retune_on_a_split_block_keeps_samples_on_the_client_sample_grid() {
        const BLOCKS: u64 = 8;
        const RETUNE_BLOCK: u64 = 3;
        // Integer, rational and two-stage slices, each on a block that splits their samples.
        for (device_rate_hz, center, client_rate, carrier, bandwidth, block) in [
            (
                3_888_000u64,
                822_520_000u64,
                388_800.0,
                821_000_000u64,
                30_000u64,
                3_888usize,
            ),
            (
                9_720_000,
                820_200_000,
                4_915_200.0,
                819_570_000,
                1_500_000,
                19_440,
            ),
            (
                12_288_000,
                821_300_000,
                800_000.0,
                815_850_000,
                200_000,
                24_576,
            ),
        ] {
            let config = CompositeConfig {
                device_rate_hz,
                rx_center_hz: center,
                ..PLAN
            };
            let plan = config
                .plan_slice(Direction::Rx, client_rate, carrier, Some(bandwidth))
                .unwrap();
            let tick_rate = device_rate_hz * 8;
            let mut fanout = RxFanout::new(device_rate_hz, tick_rate).unwrap();
            let reference = fanout.add_slot(&plan, device_rate_hz);
            let retuned = fanout.add_slot(&plan, device_rate_hz);
            // Two tones inside the passband rotate differently under a timing shift.
            let offset = plan.offset_hz as f64;
            let tones = [
                offset + bandwidth as f64 * 0.1,
                offset - bandwidth as f64 * 0.3,
            ];
            let mut outputs = (Vec::new(), Vec::new());
            for index in 0..BLOCKS {
                let start = index as usize * block;
                let device: Vec<Complex32> = (start..start + block)
                    .map(|n| {
                        tones
                            .iter()
                            .map(|hz| {
                                let phase =
                                    std::f64::consts::TAU * hz * n as f64 / device_rate_hz as f64;
                                Complex32::new(phase.cos() as f32, phase.sin() as f32)
                            })
                            .sum()
                    })
                    .collect();
                if index == RETUNE_BLOCK {
                    fanout.retune_slot(retuned, &plan, device_rate_hz).unwrap();
                }
                let ticks = tick_rate + start as u64 * 8;
                let a = fanout.process(reference, &device, ticks).samples.to_vec();
                let b = fanout.process(retuned, &device, ticks).samples.to_vec();
                if index >= BLOCKS - 2 {
                    outputs.0.extend(a);
                    outputs.1.extend(b);
                }
            }
            // The retuned slice restarts its oscillator, a constant rotation and nothing else.
            let rotation = outputs.1[0] / outputs.0[0];
            for (n, (a, b)) in outputs.0.iter().zip(&outputs.1).enumerate() {
                assert!(
                    (b - a * rotation).norm() < 0.02 * a.norm().max(1e-3),
                    "{client_rate} Hz sample {n} left the client sample grid"
                );
            }
        }
    }

    #[test]
    fn a_clock_that_is_not_a_multiple_of_the_device_rate_is_refused() {
        assert!(RxFanout::new(3_888_000, 40_000_000).is_err());
        assert!(RxFanout::new(0, 31_104_000).is_err());
        RxFanout::new(3_888_000, 31_104_000).unwrap();
    }

    #[test]
    fn each_client_gets_only_its_own_carrier_at_its_own_amplitude() {
        let mut fanout = RxFanout::new(PLAN.device_rate_hz, TICK_RATE).unwrap();
        let low = fanout.add_slot(&plan_for(821_000_000), PLAN.device_rate_hz);
        let high = fanout.add_slot(&plan_for(824_040_000), PLAN.device_rate_hz);
        assert_eq!(fanout.slots(), 2);

        let block = grid_block_samples(PLAN.device_rate_hz).unwrap();
        let device = composite(block, 0.8, 0.3);

        // Two blocks, so the second is past the filter transient. Only one slot's
        // output is borrowed at a time, which is how the bridge drains them.
        let mut measured = Vec::new();
        for pass in 0..2u64 {
            let ticks = pass * block as u64 * 8;
            let low_level = {
                let sliced = fanout.process(low, &device, ticks);
                assert_eq!(sliced.samples.len(), block / RATIO);
                baseband_magnitude(sliced.samples)
            };
            let high_level = baseband_magnitude(fanout.process(high, &device, ticks).samples);
            measured.push((low_level, high_level));
        }
        let (low_level, high_level) = measured[1];
        assert!((low_level - 0.8).abs() < 0.03, "low {low_level}");
        assert!((high_level - 0.3).abs() < 0.03, "high {high_level}");
    }

    #[test]
    fn a_retuned_slot_follows_its_new_carrier() {
        let mut fanout = RxFanout::new(PLAN.device_rate_hz, TICK_RATE).unwrap();
        let slot = fanout.add_slot(&plan_for(821_000_000), PLAN.device_rate_hz);
        let block = grid_block_samples(PLAN.device_rate_hz).unwrap();
        let device = composite(block, 0.8, 0.3);

        for _ in 0..2 {
            fanout.process(slot, &device, 0);
        }
        let low = baseband_magnitude(fanout.process(slot, &device, 0).samples);
        assert!((low - 0.8).abs() < 0.03, "before retune {low}");

        fanout
            .retune_slot(slot, &plan_for(824_040_000), PLAN.device_rate_hz)
            .unwrap();
        for _ in 0..2 {
            fanout.process(slot, &device, 0);
        }
        let high = baseband_magnitude(fanout.process(slot, &device, 0).samples);
        assert!((high - 0.3).abs() < 0.03, "after retune {high}");

        assert!(
            fanout
                .retune_slot(9, &plan_for(821_000_000), PLAN.device_rate_hz)
                .is_err()
        );
    }

    #[test]
    fn a_retired_fan_out_slot_is_reused_by_the_next_client_of_its_rate() {
        let mut fanout = RxFanout::new(PLAN.device_rate_hz, TICK_RATE).unwrap();
        let low = fanout.add_slot(&plan_for(821_000_000), PLAN.device_rate_hz);
        let high = fanout.add_slot(&plan_for(824_040_000), PLAN.device_rate_hz);
        assert_eq!(fanout.slots(), 2);

        assert!(fanout.retire_slot(low));
        assert!(!fanout.retire_slot(low));
        assert!(!fanout.retire_slot(99));

        let rejoined = fanout.add_slot(&plan_for(821_000_000), PLAN.device_rate_hz);
        assert_eq!(rejoined, low);
        assert_eq!(fanout.slots(), 2, "a rejoining client grew the fan-out");

        // The surviving peer still slices its own carrier.
        let block = grid_block_samples(PLAN.device_rate_hz).unwrap();
        let device = composite(block, 0.8, 0.3);
        for _ in 0..3 {
            fanout.process(high, &device, 0);
        }
        let level = baseband_magnitude(fanout.process(high, &device, 0).samples);
        assert!((level - 0.3).abs() < 0.03, "peer slice reads {level}");
    }

    #[test]
    fn the_reported_tick_backs_out_the_slice_group_delay() {
        let mut fanout = RxFanout::new(PLAN.device_rate_hz, TICK_RATE).unwrap();
        let slot = fanout.add_slot(&plan_for(824_040_000), PLAN.device_rate_hz);
        let block = grid_block_samples(PLAN.device_rate_hz).unwrap();
        let device = composite(block, 0.5, 0.5);

        let start_ticks = 1_000 * block as u64 * 8;
        let sliced = fanout.process(slot, &device, start_ticks);
        let delay_ticks = start_ticks - sliced.time_ticks;
        assert!(delay_ticks > 0, "no group delay backed out");
        // The delay is the filter's half-length in device samples, at eight ticks each.
        assert!(delay_ticks.is_multiple_of(8));
        assert!(
            delay_ticks < block as u64 * 8,
            "delay {delay_ticks} exceeds a block"
        );
    }

    #[test]
    fn the_first_block_clamps_rather_than_wrapping_below_the_epoch() {
        let mut fanout = RxFanout::new(PLAN.device_rate_hz, TICK_RATE).unwrap();
        let slot = fanout.add_slot(&plan_for(824_040_000), PLAN.device_rate_hz);
        let device = composite(RATIO * 4, 0.5, 0.5);
        let sliced = fanout.process(slot, &device, 0);
        assert_eq!(sliced.time_ticks, 0);
    }

    #[test]
    fn decimation_phase_holds_across_consecutive_blocks() {
        let mut fanout = RxFanout::new(PLAN.device_rate_hz, TICK_RATE).unwrap();
        let whole_slot = fanout.add_slot(&plan_for(821_000_000), PLAN.device_rate_hz);
        let split_slot = fanout.add_slot(&plan_for(821_000_000), PLAN.device_rate_hz);

        let device = composite(RATIO * 400, 1.0, 0.0);
        let all = fanout.process(whole_slot, &device, 0).samples.to_vec();

        let mut pieces = Vec::new();
        for chunk in device.chunks(RATIO * 37) {
            pieces.extend_from_slice(fanout.process(split_slot, chunk, 0).samples);
        }
        assert_eq!(all, pieces);
    }
}
