//! The shared TX timeline, where every client's samples are summed before the driver.
//!
//! Grid positions are absolute device samples from the clock epoch. Rational client
//! positions are rounded on the reduced integer rate lattice before staging.

use num_complex::Complex32;

use super::{SlicePlan, TxSlice};
use crate::error::Error;

// Clients can start 500 ms ahead, so staging must cover startup lead and a full burst.
const STAGING_WINDOW_SECONDS: u64 = 1;

/// What staging did with a client block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Staged {
    /// Placed on the grid. `rounded_samples` counts positions moved to the nearest
    /// client sample because the requested tick fell between two of them.
    Placed { rounded_samples: u64 },
    /// The grid already emitted the blocks this would have landed in.
    Late { blocks_behind: u64 },
    /// Scheduled further ahead than the staging window reaches.
    TooFarAhead { blocks_ahead: u64 },
}

/// One emitted grid block, ready for the driver.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GridBlock {
    /// Device tick of the first sample.
    pub(crate) time_ticks: u64,
    /// Summed samples at the device rate.
    pub(crate) samples: Vec<Complex32>,
}

struct Slot {
    slice: TxSlice,
    /// False until the slice has been started at a grid block.
    aligned: bool,
    ratio: usize,
    denominator: usize,
    /// False once the client holding this slot went away. A retired slot still
    /// contributes silence, so the slices around it keep their place in the grid.
    live: bool,
    staged: Vec<Complex32>,
    present: Vec<bool>,
    /// Ring position of the first client sample of `window_base_block`.
    window_start_sample: usize,
    window_base_block: u64,
    /// Client samples placed but not yet emitted, for the idle check.
    pending_samples: u64,
    late_blocks: u64,
    refused_blocks: u64,
    rounded_samples: u64,
    /// Client-sample index where the next untimed block starts.
    untimed_next: Option<u64>,
}

impl Slot {
    /// First client sample whose time falls in `block`.
    fn first_input(&self, block: u64, block_samples: usize) -> u64 {
        (block as u128 * block_samples as u128 * self.denominator as u128)
            .div_ceil(self.ratio as u128) as u64
    }

    /// Grid block a client sample's time falls in.
    fn block_of(&self, input: u64, block_samples: usize) -> u64 {
        (input as u128 * self.ratio as u128 / (self.denominator as u128 * block_samples as u128))
            as u64
    }

    /// Client samples in `block`, which differs by at most one between blocks.
    fn inputs_in(&self, block: u64, block_samples: usize) -> usize {
        (self.first_input(block + 1, block_samples) - self.first_input(block, block_samples))
            as usize
    }

    /// Ring ranges for `len` samples starting `offset` past the window start.
    fn ring_ranges(
        &self,
        offset: usize,
        len: usize,
    ) -> (std::ops::Range<usize>, std::ops::Range<usize>) {
        let start = (self.window_start_sample + offset) % self.staged.len();
        let first = len.min(self.staged.len() - start);
        (start..start + first, 0..len - first)
    }

    fn advance_to(&mut self, block: u64, block_samples: usize) {
        if block <= self.window_base_block {
            return;
        }
        let dropped = (self.first_input(block, block_samples)
            - self.first_input(self.window_base_block, block_samples))
            as usize;
        if dropped >= self.staged.len() {
            self.staged.fill(Complex32::default());
            self.present.fill(false);
            self.window_start_sample = 0;
        } else {
            let (head, tail) = self.ring_ranges(0, dropped);
            for range in [head, tail] {
                self.staged[range.clone()].fill(Complex32::default());
                self.present[range].fill(false);
            }
            self.window_start_sample = (self.window_start_sample + dropped) % self.staged.len();
        }
        self.window_base_block = block;
    }

    fn clear_staged(&mut self) {
        self.staged.fill(Complex32::default());
        self.present.fill(false);
        self.pending_samples = 0;
        self.untimed_next = None;
    }

    fn block_complete(&self, block_samples: usize) -> bool {
        let (head, tail) =
            self.ring_ranges(0, self.inputs_in(self.window_base_block, block_samples));
        self.present[head]
            .iter()
            .chain(&self.present[tail])
            .all(|present| *present)
    }

    fn take_block(&self, block_samples: usize, out: &mut Vec<Complex32>) {
        let (head, tail) =
            self.ring_ranges(0, self.inputs_in(self.window_base_block, block_samples));
        out.clear();
        out.extend_from_slice(&self.staged[head]);
        out.extend_from_slice(&self.staged[tail]);
    }
}

/// Sums every attached client into one device stream.
pub(crate) struct TxGrid {
    block_samples: usize,
    staging_blocks: usize,
    ticks_per_device_sample: u64,
    slots: Vec<Slot>,
    out: Vec<Complex32>,
    client_block: Vec<Complex32>,
    /// Next block to emit. Absent until the first client block is staged.
    next_block: Option<u64>,
}

impl TxGrid {
    /// Creates an empty grid.
    ///
    /// Returns an error unless `tick_rate` is a whole multiple of `device_rate_hz`, and
    /// unless `block_samples` is positive.
    pub(crate) fn new(
        device_rate_hz: u64,
        tick_rate: u64,
        block_samples: usize,
    ) -> Result<Self, Error> {
        if device_rate_hz == 0 || block_samples == 0 {
            return Err("a composite grid needs a positive device rate and block length".into());
        }
        if !tick_rate.is_multiple_of(device_rate_hz) {
            return Err(format!(
                "composite mode needs a device clock that is a whole multiple of the device rate, and {tick_rate} Hz is not a multiple of {device_rate_hz} Hz"
            )
            .into());
        }
        Ok(Self {
            block_samples,
            staging_blocks: usize::try_from(
                device_rate_hz
                    .saturating_mul(STAGING_WINDOW_SECONDS)
                    .div_ceil(block_samples as u64),
            )
            .map_err(|_| Error::from("composite staging window exceeds platform usize"))?,
            ticks_per_device_sample: tick_rate / device_rate_hz,
            slots: Vec::new(),
            out: vec![Complex32::default(); block_samples],
            client_block: Vec::new(),
            next_block: None,
        })
    }

    /// Adds a client and returns its slot index, reusing a retired slot of the same rate
    /// ratio so a client that comes and goes cannot grow the grid without bound.
    pub(crate) fn add_slot(&mut self, plan: &SlicePlan, device_rate_hz: u64, gain: f32) -> usize {
        if let Some(index) = self.slots.iter().position(|slot| {
            !slot.live && slot.ratio == plan.ratio && slot.denominator == plan.denominator
        }) {
            let slot = &mut self.slots[index];
            slot.slice = TxSlice::new(plan, device_rate_hz, gain);
            slot.aligned = false;
            slot.live = true;
            slot.untimed_next = None;
            slot.late_blocks = 0;
            slot.refused_blocks = 0;
            slot.rounded_samples = 0;
            return index;
        }
        let per_block = (self.block_samples * plan.denominator).div_ceil(plan.ratio);
        self.slots.push(Slot {
            slice: TxSlice::new(plan, device_rate_hz, gain),
            aligned: false,
            ratio: plan.ratio,
            denominator: plan.denominator,
            live: true,
            staged: vec![Complex32::default(); per_block * self.staging_blocks],
            present: vec![false; per_block * self.staging_blocks],
            window_start_sample: 0,
            window_base_block: self.next_block.unwrap_or(0),
            pending_samples: 0,
            late_blocks: 0,
            refused_blocks: 0,
            rounded_samples: 0,
            untimed_next: None,
        });
        self.slots.len() - 1
    }

    /// Releases a slot whose client went away, discarding whatever it had staged.
    ///
    /// The slot keeps its place so its peers' indices stay valid, and contributes silence
    /// until another client of the same rate ratio claims it.
    pub(crate) fn retire_slot(&mut self, slot_index: usize) -> bool {
        let Some(slot) = self.slots.get_mut(slot_index) else {
            return false;
        };
        if !slot.live {
            return false;
        }
        slot.live = false;
        slot.clear_staged();
        true
    }

    /// Replaces one slot's slice, keeping its staged samples and its place in the grid.
    ///
    /// The new slice starts with zero filter history and zero oscillator phase, which is
    /// what a retune is. The rate ratio cannot change, because the slot's staging window
    /// is sized for it.
    pub(crate) fn retune_slot(
        &mut self,
        slot_index: usize,
        plan: &SlicePlan,
        device_rate_hz: u64,
        gain: f32,
    ) -> Result<(), Error> {
        let slot = self.slots.get_mut(slot_index).ok_or_else(|| -> Error {
            format!("composite TX slot {slot_index} is not open").into()
        })?;
        if plan.ratio != slot.ratio || plan.denominator != slot.denominator {
            return Err(format!(
                "composite TX slot {slot_index} runs at ratio {}, so it cannot retune to ratio {}",
                slot.ratio, plan.ratio
            )
            .into());
        }
        slot.slice = TxSlice::new(plan, device_rate_hz, gain);
        slot.aligned = false;
        Ok(())
    }

    /// Slots added so far, which bounds the slot indices diagnostics can name.
    pub(crate) fn slots_added(&self) -> usize {
        self.slots.len()
    }

    #[cfg(test)]
    pub(crate) fn block_samples(&self) -> usize {
        self.block_samples
    }

    /// Device ticks one grid block spans.
    pub(crate) fn block_ticks(&self) -> u64 {
        self.block_samples as u64 * self.ticks_per_device_sample
    }

    /// Grid block a device tick falls in.
    pub(crate) fn block_at_ticks(&self, time_ticks: u64) -> u64 {
        time_ticks / self.ticks_per_device_sample / self.block_samples as u64
    }

    pub(crate) fn reset_timeline(&mut self) {
        self.next_block = None;
        for slot in &mut self.slots {
            slot.clear_staged();
            slot.aligned = false;
            slot.window_start_sample = 0;
            slot.window_base_block = 0;
        }
    }

    pub(crate) fn silence_slot(&mut self, index: usize) {
        if let Some(slot) = self.slots.get_mut(index) {
            slot.clear_staged();
        }
    }

    /// Places one client's samples on the grid.
    ///
    /// `time_ticks` is the device tick of `samples[0]`. A tick between two of the
    /// client's own sample positions is rounded to the nearest, which is under one client
    /// sample and is counted rather than hidden.
    pub(crate) fn stage(
        &mut self,
        slot_index: usize,
        time_ticks: u64,
        samples: &[Complex32],
    ) -> Staged {
        let ticks_per_client_sample = self.ticks_per_client_sample(slot_index);
        let position = time_ticks as u128 * self.slots[slot_index].denominator as u128;
        // Absolute client-sample index, rounded to the nearest whole position.
        let rounded = ((position + ticks_per_client_sample as u128 / 2)
            / ticks_per_client_sample as u128) as u64;
        let rounded_samples = u64::from(!position.is_multiple_of(ticks_per_client_sample as u128));
        self.place(slot_index, rounded, rounded_samples, samples)
    }

    /// Places an untimed block after the slot's previous one, or from `earliest_ticks`
    /// when there is none or its place already went out.
    pub(crate) fn stage_untimed(
        &mut self,
        slot_index: usize,
        earliest_ticks: u64,
        samples: &[Complex32],
    ) -> Staged {
        let ticks_per_client_sample = self.ticks_per_client_sample(slot_index);
        let block_samples = self.block_samples;
        let slot = &self.slots[slot_index];
        let earliest = (earliest_ticks as u128 * slot.denominator as u128)
            .div_ceil(ticks_per_client_sample as u128) as u64;
        let base = self
            .next_block
            .unwrap_or(slot.block_of(earliest, block_samples));
        let start = slot
            .untimed_next
            .filter(|&next| slot.block_of(next, block_samples) >= base)
            .unwrap_or(earliest.max(slot.first_input(base, block_samples)));
        let outcome = self.place(slot_index, start, 0, samples);
        if matches!(outcome, Staged::Placed { .. }) {
            self.slots[slot_index].untimed_next = Some(start + samples.len() as u64);
        }
        outcome
    }

    /// Device ticks per client sample, scaled by the slot's denominator.
    fn ticks_per_client_sample(&self, slot_index: usize) -> u64 {
        self.ticks_per_device_sample * self.slots[slot_index].ratio as u64
    }

    fn place(
        &mut self,
        slot_index: usize,
        rounded: u64,
        rounded_samples: u64,
        samples: &[Complex32],
    ) -> Staged {
        let block_samples = self.block_samples;
        let first_block = self.slots[slot_index].block_of(rounded, block_samples);
        let base = self.next_block.unwrap_or(first_block);
        if self.next_block.is_none() {
            self.next_block = Some(base);
            for slot in &mut self.slots {
                slot.window_base_block = base;
            }
        }
        let slot = &mut self.slots[slot_index];
        slot.advance_to(base, block_samples);

        if !slot.live {
            return Staged::Late { blocks_behind: 0 };
        }
        if first_block < base {
            slot.late_blocks += 1;
            return Staged::Late {
                blocks_behind: base - first_block,
            };
        }
        let window_end = base + self.staging_blocks as u64;
        let last_index = rounded + samples.len().saturating_sub(1) as u64;
        let last_block = slot.block_of(last_index, block_samples);
        if last_block >= window_end {
            slot.refused_blocks += 1;
            return Staged::TooFarAhead {
                blocks_ahead: last_block - base,
            };
        }

        let offset = (rounded - slot.first_input(base, block_samples)) as usize;
        let (head, tail) = slot.ring_ranges(offset, samples.len());
        let (first, rest) = samples.split_at(head.len());
        slot.staged[head.clone()].copy_from_slice(first);
        slot.present[head].fill(true);
        slot.staged[tail.clone()].copy_from_slice(rest);
        slot.present[tail].fill(true);
        slot.pending_samples = slot.pending_samples.saturating_add(samples.len() as u64);
        slot.rounded_samples += rounded_samples;
        Staged::Placed { rounded_samples }
    }

    pub(crate) fn emit_available(
        &mut self,
        commit_block: u64,
        deadline_block: u64,
        enabled: impl Fn(usize) -> bool,
    ) -> Option<GridBlock> {
        let block = self.next_block?;
        if block > commit_block {
            return None;
        }
        if block > deadline_block
            && self.slots.iter().enumerate().any(|(index, slot)| {
                slot.live && enabled(index) && !slot.block_complete(self.block_samples)
            })
        {
            return None;
        }
        self.emit(commit_block)
    }

    /// Emits the next grid block when it is at or before `commit_block`.
    ///
    /// Every slot contributes to every emitted block, silence included, which keeps each
    /// slice's filter history and oscillator phase locked to device time.
    pub(crate) fn emit(&mut self, commit_block: u64) -> Option<GridBlock> {
        let block = self.next_block?;
        if block > commit_block {
            return None;
        }
        self.out.fill(Complex32::default());
        let block_samples = self.block_samples;
        for slot in &mut self.slots {
            slot.advance_to(block, block_samples);
            // A retired slot's filter state no longer has to stay aligned with device
            // time, so it is skipped rather than fed silence.
            if slot.live {
                if !slot.aligned {
                    let device_sample = block * block_samples as u64;
                    let first_input = slot.first_input(block, block_samples);
                    slot.slice.align(device_sample, first_input);
                    slot.aligned = true;
                }
                slot.take_block(block_samples, &mut self.client_block);
                slot.slice
                    .accumulate_into(&self.client_block, &mut self.out);
                slot.pending_samples = slot
                    .pending_samples
                    .saturating_sub(self.client_block.len() as u64);
            }
            slot.advance_to(block + 1, block_samples);
        }
        self.next_block = Some(block + 1);
        Some(GridBlock {
            time_ticks: block * self.block_ticks(),
            samples: self.out.clone(),
        })
    }

    /// Per-slot counts of blocks that missed their place and samples that were rounded.
    pub(crate) fn slot_diagnostics(&self, slot_index: usize) -> (u64, u64, u64) {
        let slot = &self.slots[slot_index];
        (slot.late_blocks, slot.refused_blocks, slot.rounded_samples)
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
    /// 3.888 MHz times eight, a clock the implicit selection would pick.
    const TICK_RATE: u64 = 31_104_000;
    const CLIENT_RATE_HZ: u64 = 388_800;
    const RATIO: usize = 10;

    fn plan_for(carrier_hz: u64) -> SlicePlan {
        PLAN.plan_slice(
            Direction::Tx,
            CLIENT_RATE_HZ as f64,
            carrier_hz,
            Some(30_000),
        )
        .unwrap()
    }

    fn grid() -> TxGrid {
        let block = grid_block_samples(PLAN.device_rate_hz).unwrap();
        TxGrid::new(PLAN.device_rate_hz, TICK_RATE, block).unwrap()
    }

    fn dc(len: usize) -> Vec<Complex32> {
        vec![Complex32::new(1.0, 0.0); len]
    }

    #[test]
    fn large_unaligned_upstream_batches_survive_small_output_blocks() {
        const RATE: u64 = 9_720_000;
        const OUTPUT_BLOCK: usize = 8_100;
        const CLIENT_BLOCK: usize = 4_096;
        const UPSTREAM_BATCH: usize = 24_576;
        const CLIENT_OFFSET: usize = 1_033;
        let config = super::super::CompositeConfig {
            device_rate_hz: RATE,
            tx_center_hz: 865_200_000,
            rx_center_hz: 820_200_000,
            tx_digital_backoff_db: 0.0,
            block_duration_ms: 5.0 / 6.0,
            tx_deadline_ms: 2.0,
        };
        let plan = config
            .plan_slice(
                super::super::Direction::Tx,
                4_915_200.0,
                864_570_000,
                Some(1_500_000),
            )
            .unwrap();
        let mut grid = TxGrid::new(RATE, RATE, OUTPUT_BLOCK).unwrap();
        let slot = grid.add_slot(&plan, RATE, 1.0);
        let input = (0..UPSTREAM_BATCH * 2)
            .map(|n| Complex32::new((n % 29) as f32 / 29.0, (n % 17) as f32 / 17.0))
            .collect::<Vec<_>>();
        for (batch, samples) in input.chunks(UPSTREAM_BATCH).enumerate() {
            let client_sample = (CLIENT_OFFSET + batch * UPSTREAM_BATCH) as u64;
            let tick = RATE + (client_sample * 2025 + 512) / 1024;
            assert!(matches!(
                grid.stage(slot, tick, samples),
                Staged::Placed { .. }
            ));
        }
        let count = (CLIENT_OFFSET + input.len()).div_ceil(CLIENT_BLOCK);
        let mut padded = vec![Complex32::default(); CLIENT_OFFSET];
        padded.extend_from_slice(&input);
        padded.resize(count * CLIENT_BLOCK, Complex32::default());
        let mut expected = vec![Complex32::default(); count * OUTPUT_BLOCK];
        TxSlice::new(&plan, RATE, 1.0).accumulate_into(&padded, &mut expected);
        let first_block = RATE / OUTPUT_BLOCK as u64;
        let mut actual = Vec::new();
        for block in first_block..first_block + count as u64 {
            let output = grid.emit(block).unwrap();
            assert_eq!(output.time_ticks, block * OUTPUT_BLOCK as u64);
            actual.extend_from_slice(&output.samples);
        }
        assert_eq!(actual.len(), expected.len());
        let maximum_error = actual
            .iter()
            .zip(expected)
            .map(|(a, b)| (*a - b).norm())
            .fold(0.0f32, f32::max);
        assert!(maximum_error < 1e-5, "waveform error={maximum_error}");
    }

    #[test]
    fn rational_tx_fragments_land_without_sample_gaps() {
        const RATE: u64 = 9_720_000;
        const BLOCK: usize = 48_600;
        let config = super::super::CompositeConfig {
            device_rate_hz: RATE,
            tx_center_hz: 865_200_000,
            rx_center_hz: 820_200_000,
            tx_digital_backoff_db: 0.0,
            block_duration_ms: 5.0 / 6.0,
            tx_deadline_ms: 2.0,
        };
        let plan = config
            .plan_slice(
                super::super::Direction::Tx,
                4_915_200.0,
                864_570_000,
                Some(1_500_000),
            )
            .unwrap();
        let mut grid = TxGrid::new(RATE, RATE, BLOCK).unwrap();
        let slot = grid.add_slot(&plan, RATE, 1.0);
        let samples = (0..24_576)
            .map(|n| Complex32::new(n as f32 / 24_576.0, 0.0))
            .collect::<Vec<_>>();
        let mut offset = 0u64;
        for chunk in samples.chunks(173) {
            let tick = RATE + (offset * 2025 + 512) / 1024;
            assert!(matches!(
                grid.stage(slot, tick, chunk),
                Staged::Placed { .. }
            ));
            offset += chunk.len() as u64;
        }
        let mut expected = vec![Complex32::default(); BLOCK];
        TxSlice::new(&plan, RATE, 1.0).accumulate_into(&samples, &mut expected);
        let actual = grid.emit(RATE / BLOCK as u64).unwrap();
        assert_eq!(actual.time_ticks, RATE);
        assert_eq!(actual.samples, expected);
    }

    #[test]
    fn incomplete_blocks_wait_for_client_samples_before_the_hardware_deadline() {
        let plan = PLAN
            .plan_slice(
                Direction::Tx,
                CLIENT_RATE_HZ as f64,
                866_000_000,
                Some(30_000),
            )
            .unwrap();
        let block_samples = grid_block_samples(PLAN.device_rate_hz).unwrap();
        let mut grid = TxGrid::new(PLAN.device_rate_hz, TICK_RATE, block_samples).unwrap();
        let first = grid.add_slot(&plan, PLAN.device_rate_hz, 1.0);
        let second = grid.add_slot(&plan, PLAN.device_rate_hz, 1.0);
        let block = 10;
        let ticks = block * grid.block_ticks();
        let samples = dc(block_samples / plan.ratio);
        grid.stage(first, ticks, &samples);
        assert!(grid.emit_available(block, block - 1, |_| true).is_none());
        grid.stage(second, ticks, &samples);
        assert_eq!(
            grid.emit_available(block, block - 1, |_| true)
                .unwrap()
                .time_ticks,
            ticks
        );
        let next = block + 1;
        grid.stage(first, next * grid.block_ticks(), &samples);
        assert!(grid.emit_available(next, next - 1, |_| true).is_none());
        assert!(grid.emit_available(next, next, |_| true).is_some());
    }

    #[test]
    fn a_clock_that_is_not_a_multiple_of_the_device_rate_is_refused() {
        let Err(error) = TxGrid::new(3_888_000, 40_000_000, 3_240) else {
            panic!("a clock that does not divide the device rate is refused");
        };
        let message = error.to_string();
        assert!(message.contains("whole multiple"), "{message}");
        TxGrid::new(3_888_000, 31_104_000, 3_240).unwrap();
        // The largest multiple under a B210 ceiling also works.
        TxGrid::new(3_888_000, 58_320_000, 3_240).unwrap();
        assert!(TxGrid::new(0, 31_104_000, 3_240).is_err());
        assert!(TxGrid::new(3_888_000, 31_104_000, 0).is_err());
    }

    #[test]
    fn grid_geometry_follows_the_device_rate_and_clock() {
        let grid = grid();
        assert_eq!(grid.block_samples(), 3_240);
        // 19,440 device samples at eight ticks each.
        assert_eq!(grid.block_ticks(), 25_920);
        assert_eq!(grid.block_at_ticks(0), 0);
        assert_eq!(grid.block_at_ticks(25_919), 0);
        assert_eq!(grid.block_at_ticks(25_920), 1);
        assert_eq!(grid.block_at_ticks(51_840), 2);
    }

    #[test]
    fn a_retuned_slot_moves_its_carrier_and_keeps_its_place() {
        let mut grid = grid();
        let slot = grid.add_slot(&plan_for(867_520_000), PLAN.device_rate_hz, 1.0);
        let per_block = 3_240 / RATIO;
        let magnitude = |samples: &[Complex32], offset_hz: i64| -> f32 {
            let step = -std::f64::consts::TAU * offset_hz as f64 / PLAN.device_rate_hz as f64;
            let mut sum = Complex32::default();
            for (n, &sample) in samples.iter().enumerate() {
                let phase = step * n as f64;
                sum += sample * Complex32::new(phase.cos() as f32, phase.sin() as f32);
            }
            sum.norm() / samples.len() as f32
        };

        // Centered to begin with, as a client that has not tuned yet would be.
        for block in 0..4u64 {
            grid.stage(slot, block * grid.block_ticks(), &dc(per_block));
            let out = grid.emit(block).expect("block");
            if block == 3 {
                assert!(magnitude(&out.samples, 0) > 0.95);
            }
        }

        grid.retune_slot(slot, &plan_for(869_040_000), PLAN.device_rate_hz, 1.0)
            .unwrap();
        for block in 4..8u64 {
            grid.stage(slot, block * grid.block_ticks(), &dc(per_block));
            let out = grid.emit(block).expect("block");
            assert_eq!(out.time_ticks, block * grid.block_ticks());
            if block == 7 {
                assert!(magnitude(&out.samples, 1_520_000) > 0.95);
                assert!(magnitude(&out.samples, 0) < 0.01);
            }
        }
    }

    #[test]
    fn a_retired_slot_goes_silent_and_is_reused_by_the_next_client() {
        let mut grid = grid();
        let per_block = 3_240 / RATIO;
        let low = grid.add_slot(&plan_for(866_000_000), PLAN.device_rate_hz, 1.0);
        let high = grid.add_slot(&plan_for(869_040_000), PLAN.device_rate_hz, 1.0);

        // The low client stages a block and then goes away before it is emitted.
        grid.stage(low, 0, &dc(per_block));
        grid.stage(high, 0, &dc(per_block));
        assert!(grid.retire_slot(low));
        // Retiring is not repeatable, so a second release reports nothing to release.
        assert!(!grid.retire_slot(low));
        assert!(!grid.retire_slot(99));

        let magnitude = |samples: &[Complex32], offset_hz: i64| -> f32 {
            let step = -std::f64::consts::TAU * offset_hz as f64 / PLAN.device_rate_hz as f64;
            let mut sum = Complex32::default();
            for (n, &sample) in samples.iter().enumerate() {
                let phase = step * n as f64;
                sum += sample * Complex32::new(phase.cos() as f32, phase.sin() as f32);
            }
            sum.norm() / samples.len() as f32
        };
        let block = grid.emit(0).expect("block");
        // Its staged samples went with it, and its peer is untouched.
        assert!(magnitude(&block.samples, -1_520_000) < 0.01);
        assert!(magnitude(&block.samples, 1_520_000) > 0.5);

        // A retired slot refuses new samples rather than resurrecting itself.
        assert_eq!(
            grid.stage(low, grid.block_ticks(), &dc(per_block)),
            Staged::Late { blocks_behind: 0 }
        );

        // The next client of the same rate takes that slot rather than a new one.
        let rejoined = grid.add_slot(&plan_for(866_000_000), PLAN.device_rate_hz, 1.0);
        assert_eq!(rejoined, low);
        assert_eq!(grid.slots_added(), 2);
        // The reclaimed slot transmits again at its own offset. Blocks come out in
        // order, so the staged one arrives after the empty blocks before it.
        let staged_block = 4u64;
        grid.stage(rejoined, staged_block * grid.block_ticks(), &dc(per_block));
        let mut reclaimed = None;
        while let Some(block) = grid.emit(staged_block) {
            if block.time_ticks == staged_block * grid.block_ticks() {
                reclaimed = Some(block);
            }
        }
        let reclaimed = reclaimed.expect("the staged block came out");
        assert!(magnitude(&reclaimed.samples, -1_520_000) > 0.5);
    }

    #[test]
    fn a_slot_of_a_different_rate_is_not_reused() {
        let mut grid = grid();
        let fast = grid.add_slot(&plan_for(866_000_000), PLAN.device_rate_hz, 1.0);
        grid.retire_slot(fast);
        let slow_plan = PLAN
            .plan_slice(Direction::Tx, 97_200.0, 869_040_000, None)
            .unwrap();
        // A 97,200 Hz client cannot inherit a 388,800 Hz client's staging window.
        let slow = grid.add_slot(&slow_plan, PLAN.device_rate_hz, 1.0);
        assert_ne!(slow, fast);
        assert_eq!(grid.slots_added(), 2);
    }

    #[test]
    fn a_retune_cannot_change_a_slot_rate_or_reach_a_slot_that_is_not_open() {
        let mut grid = grid();
        let slot = grid.add_slot(&plan_for(867_520_000), PLAN.device_rate_hz, 1.0);
        let other_rate = PLAN
            .plan_slice(Direction::Tx, 97_200.0, 867_520_000, None)
            .unwrap();
        let message = grid
            .retune_slot(slot, &other_rate, PLAN.device_rate_hz, 1.0)
            .expect_err("a rate change is not a retune")
            .to_string();
        assert!(message.contains("cannot retune to ratio"), "{message}");
        assert!(
            grid.retune_slot(9, &plan_for(869_040_000), PLAN.device_rate_hz, 1.0)
                .is_err()
        );
    }

    #[test]
    fn nothing_is_emitted_before_a_client_stages_anything() {
        let mut grid = grid();
        grid.add_slot(&plan_for(869_040_000), PLAN.device_rate_hz, 1.0);
        assert_eq!(grid.slots_added(), 1);
        assert!(grid.emit(u64::MAX).is_none());
    }

    #[test]
    fn a_staged_block_emits_at_its_own_tick() {
        let mut grid = grid();
        let slot = grid.add_slot(&plan_for(869_040_000), PLAN.device_rate_hz, 1.0);
        let start_ticks = 100 * grid.block_ticks();
        assert_eq!(
            grid.stage(slot, start_ticks, &dc(3_240 / RATIO)),
            Staged::Placed { rounded_samples: 0 }
        );

        // A commit horizon before the block holds it.
        assert!(grid.emit(99).is_none());
        let block = grid.emit(100).expect("block at its own position");
        assert_eq!(block.time_ticks, start_ticks);
        assert_eq!(block.samples.len(), 3_240);
        assert!(grid.emit(100).is_none());
    }

    #[test]
    fn two_clients_land_in_the_same_block_at_their_own_offsets() {
        let mut grid = grid();
        let low = grid.add_slot(&plan_for(866_000_000), PLAN.device_rate_hz, 0.5);
        let high = grid.add_slot(&plan_for(869_040_000), PLAN.device_rate_hz, 0.25);
        assert_eq!(grid.slots_added(), 2);

        let per_block = 3_240 / RATIO;
        let mut emitted = Vec::new();
        for block in 0..4u64 {
            let ticks = block * grid.block_ticks();
            grid.stage(low, ticks, &dc(per_block));
            grid.stage(high, ticks, &dc(per_block));
            if let Some(out) = grid.emit(block) {
                emitted.push(out);
            }
        }
        assert_eq!(emitted.len(), 4);

        let last = &emitted[3];
        let magnitude = |offset_hz: i64| -> f32 {
            let step = -std::f64::consts::TAU * offset_hz as f64 / PLAN.device_rate_hz as f64;
            let mut sum = Complex32::default();
            for (n, &sample) in last.samples.iter().enumerate() {
                let phase = step * n as f64;
                sum += sample * Complex32::new(phase.cos() as f32, phase.sin() as f32);
            }
            sum.norm() / last.samples.len() as f32
        };
        assert!((magnitude(-1_520_000) - 0.5).abs() < 0.02);
        assert!((magnitude(1_520_000) - 0.25).abs() < 0.02);
    }

    #[test]
    fn one_client_falling_silent_still_emits_the_other() {
        let mut grid = grid();
        let low = grid.add_slot(&plan_for(866_000_000), PLAN.device_rate_hz, 1.0);
        let high = grid.add_slot(&plan_for(869_040_000), PLAN.device_rate_hz, 1.0);
        let per_block = 3_240 / RATIO;

        let mut blocks = Vec::new();
        for block in 0..4u64 {
            let ticks = block * grid.block_ticks();
            grid.stage(low, ticks, &dc(per_block));
            // The high slot supplies nothing for block two.
            if block != 2 {
                grid.stage(high, ticks, &dc(per_block));
            }
            blocks.push(grid.emit(block).expect("a block per position"));
        }
        // Every block still went out, in order, at its own tick.
        for (index, block) in blocks.iter().enumerate() {
            assert_eq!(block.time_ticks, index as u64 * grid.block_ticks());
            assert_eq!(block.samples.len(), 3_240);
        }
        assert!(blocks[2].samples.iter().any(|s| s.norm() > 0.1));
    }

    #[test]
    fn untimed_blocks_follow_each_other_from_the_first_anchor() {
        let mut grid = grid();
        let slot = grid.add_slot(&plan_for(869_040_000), PLAN.device_rate_hz, 1.0);
        let per_block = 3_240 / RATIO;
        let start = 100 * grid.block_ticks();
        assert_eq!(
            grid.stage_untimed(slot, start, &dc(per_block)),
            Staged::Placed { rounded_samples: 0 }
        );
        grid.stage_untimed(slot, start + 5 * grid.block_ticks(), &dc(per_block));
        assert_eq!(
            grid.slots[slot].untimed_next,
            Some(100 * per_block as u64 + 2 * per_block as u64)
        );
        assert_eq!(grid.emit(100).expect("first block").time_ticks, start);
        assert!(grid.emit(101).is_some());
    }

    #[test]
    fn an_untimed_stream_whose_place_went_out_restarts_at_the_earliest_tick() {
        let mut grid = grid();
        let slot = grid.add_slot(&plan_for(869_040_000), PLAN.device_rate_hz, 1.0);
        let per_block = 3_240 / RATIO;
        grid.stage_untimed(slot, 10 * grid.block_ticks(), &dc(per_block));
        for block in 10..13 {
            grid.emit(block);
        }
        assert_eq!(
            grid.stage_untimed(slot, 20 * grid.block_ticks(), &dc(per_block)),
            Staged::Placed { rounded_samples: 0 }
        );
        assert_eq!(grid.slots[slot].untimed_next, Some(21 * per_block as u64));
    }

    #[test]
    fn an_untimed_anchor_behind_the_emitted_grid_starts_at_the_next_block() {
        let mut grid = grid();
        let timed = grid.add_slot(&plan_for(866_000_000), PLAN.device_rate_hz, 1.0);
        let untimed = grid.add_slot(&plan_for(869_040_000), PLAN.device_rate_hz, 1.0);
        let per_block = 3_240 / RATIO;
        grid.stage(timed, 50 * grid.block_ticks(), &dc(per_block));
        grid.emit(50);
        assert_eq!(
            grid.stage_untimed(untimed, 40 * grid.block_ticks(), &dc(per_block)),
            Staged::Placed { rounded_samples: 0 }
        );
        assert_eq!(
            grid.slots[untimed].untimed_next,
            Some(52 * per_block as u64)
        );
    }

    #[test]
    fn a_refused_untimed_block_leaves_its_place_for_the_next_one() {
        let mut grid = grid();
        let slot = grid.add_slot(&plan_for(869_040_000), PLAN.device_rate_hz, 1.0);
        let per_block = 3_240 / RATIO;
        grid.stage_untimed(slot, 0, &dc(per_block));
        let too_long = dc(per_block * (grid.staging_blocks + 1));
        assert!(matches!(
            grid.stage_untimed(slot, 0, &too_long),
            Staged::TooFarAhead { .. }
        ));
        assert_eq!(grid.slots[slot].untimed_next, Some(per_block as u64));
    }

    #[test]
    fn silencing_or_retiring_a_slot_forgets_its_untimed_position() {
        let mut grid = grid();
        let slot = grid.add_slot(&plan_for(869_040_000), PLAN.device_rate_hz, 1.0);
        let per_block = 3_240 / RATIO;
        grid.stage_untimed(slot, 0, &dc(per_block));
        grid.silence_slot(slot);
        assert_eq!(grid.slots[slot].untimed_next, None);
        grid.stage_untimed(slot, 0, &dc(per_block));
        grid.retire_slot(slot);
        assert_eq!(grid.slots[slot].untimed_next, None);
        let reused = grid.add_slot(&plan_for(869_040_000), PLAN.device_rate_hz, 1.0);
        assert_eq!(reused, slot);
        assert_eq!(grid.slots[slot].untimed_next, None);
    }

    #[test]
    fn a_block_behind_the_grid_is_reported_late_rather_than_placed() {
        let mut grid = grid();
        let slot = grid.add_slot(&plan_for(869_040_000), PLAN.device_rate_hz, 1.0);
        let per_block = 3_240 / RATIO;
        grid.stage(slot, 10 * grid.block_ticks(), &dc(per_block));
        grid.emit(10).expect("first block");
        grid.emit(11);

        let outcome = grid.stage(slot, 10 * grid.block_ticks(), &dc(per_block));
        assert_eq!(outcome, Staged::Late { blocks_behind: 2 });
        let (late, refused, _) = grid.slot_diagnostics(slot);
        assert_eq!((late, refused), (1, 0));
    }

    #[test]
    fn established_grid_accepts_100ms_and_500ms_client_leads() {
        let mut grid = grid();
        let first = grid.add_slot(&plan_for(867_520_000), PLAN.device_rate_hz, 1.0);
        let peer = grid.add_slot(&plan_for(869_040_000), PLAN.device_rate_hz, 1.0);
        let per_block = grid.block_samples() / RATIO;
        assert_eq!(
            grid.stage(first, 0, &dc(per_block)),
            Staged::Placed { rounded_samples: 0 }
        );
        grid.emit(0).unwrap();
        for (slot, lead_ms) in [(first, 100), (peer, 500)] {
            let tick = TICK_RATE * lead_ms / 1000;
            assert_eq!(
                grid.stage(slot, tick, &dc(per_block * 3)),
                Staged::Placed { rounded_samples: 0 },
                "client lead {lead_ms} ms"
            );
        }
    }

    #[test]
    fn scheduled_samples_survive_repeated_window_wraps_without_replaying_old_samples() {
        let mut grid = grid();
        let plan = plan_for(867_520_000);
        let slot = grid.add_slot(&plan, PLAN.device_rate_hz, 1.0);
        let mut reference = TxSlice::new(&plan, PLAN.device_rate_hz, 1.0);
        let per_block = grid.block_samples() / RATIO;
        let lookahead_blocks = 20;
        let writes = grid.staging_blocks as u64 * 2;
        let pattern = |block: u64| match block % 3 {
            0 => Complex32::new(0.25, -0.5),
            1 => Complex32::new(-0.5, 0.25),
            _ => Complex32::default(),
        };
        grid.stage(slot, 0, &vec![Complex32::default(); per_block]);
        for block in 0..writes + grid.staging_blocks as u64 {
            if block < writes {
                let samples: Vec<_> = (0..3)
                    .flat_map(|offset| {
                        std::iter::repeat_n(pattern(block + lookahead_blocks + offset), per_block)
                    })
                    .collect();
                assert_eq!(
                    grid.stage(
                        slot,
                        (block + lookahead_blocks) * grid.block_ticks(),
                        &samples,
                    ),
                    Staged::Placed { rounded_samples: 0 }
                );
            }
            let sample = if (lookahead_blocks..writes + lookahead_blocks + 2).contains(&block) {
                pattern(block)
            } else {
                Complex32::default()
            };
            let mut expected = vec![Complex32::default(); grid.block_samples()];
            reference.accumulate_into(&vec![sample; per_block], &mut expected);
            let emitted = grid.emit(block).unwrap();
            assert_eq!(emitted.time_ticks, block * grid.block_ticks());
            assert_eq!(emitted.samples, expected, "grid block {block}");
        }
    }

    #[test]
    fn a_block_past_the_staging_window_is_refused_rather_than_buffered() {
        let mut grid = grid();
        let slot = grid.add_slot(&plan_for(869_040_000), PLAN.device_rate_hz, 1.0);
        let per_block = 3_240 / RATIO;
        grid.stage(slot, 0, &dc(per_block));

        let staging_blocks = grid.staging_blocks as u64;
        let far = staging_blocks * grid.block_ticks();
        let outcome = grid.stage(slot, far, &dc(per_block));
        assert_eq!(
            outcome,
            Staged::TooFarAhead {
                blocks_ahead: staging_blocks
            }
        );
        let edge = (staging_blocks - 1) * grid.block_ticks();
        assert_eq!(
            grid.stage(slot, edge, &dc(per_block)),
            Staged::Placed { rounded_samples: 0 }
        );
        let (late, refused, _) = grid.slot_diagnostics(slot);
        assert_eq!((late, refused), (0, 1));
    }

    #[test]
    fn a_tick_between_client_samples_is_rounded_and_counted() {
        let mut grid = grid();
        let slot = grid.add_slot(&plan_for(869_040_000), PLAN.device_rate_hz, 1.0);
        // Eighty ticks per client sample, so four ticks is under half a sample.
        let ticks_per_client_sample = 8 * RATIO as u64;
        let outcome = grid.stage(slot, ticks_per_client_sample * 5 + 4, &dc(16));
        assert_eq!(outcome, Staged::Placed { rounded_samples: 1 });
        let (_, _, rounded) = grid.slot_diagnostics(slot);
        assert_eq!(rounded, 1);

        let aligned = grid.stage(slot, ticks_per_client_sample * 9, &dc(16));
        assert_eq!(aligned, Staged::Placed { rounded_samples: 0 });
    }

    #[test]
    fn a_block_spanning_two_grid_blocks_emits_across_both() {
        let mut grid = grid();
        let slot = grid.add_slot(&plan_for(867_520_000), PLAN.device_rate_hz, 1.0);
        let per_block = 3_240 / RATIO;
        // Start half a block in and run a block and a half.
        let ticks = grid.block_ticks() / 2;
        grid.stage(slot, ticks, &dc(per_block + per_block / 2));

        let first = grid.emit(0).expect("first block");
        let second = grid.emit(1).expect("second block");
        assert_eq!(first.time_ticks, 0);
        assert_eq!(second.time_ticks, grid.block_ticks());
        // The first half of block zero is silence and the rest is signal.
        let head: f32 = first.samples[..per_block * RATIO / 4]
            .iter()
            .map(|s| s.norm())
            .sum();
        let tail: f32 = first.samples[per_block * RATIO * 3 / 4..]
            .iter()
            .map(|s| s.norm())
            .sum();
        assert!(head < 1.0, "head energy {head}");
        assert!(tail > 100.0, "tail energy {tail}");
        assert!(second.samples.iter().any(|s| s.norm() > 0.1));
    }

    #[test]
    fn a_slot_added_after_the_grid_started_joins_at_the_current_block() {
        let mut grid = grid();
        let first = grid.add_slot(&plan_for(866_000_000), PLAN.device_rate_hz, 1.0);
        let per_block = 3_240 / RATIO;
        grid.stage(first, 50 * grid.block_ticks(), &dc(per_block));
        grid.emit(50).expect("first block");

        let joined = grid.add_slot(&plan_for(869_040_000), PLAN.device_rate_hz, 1.0);
        let ticks = 51 * grid.block_ticks();
        assert_eq!(
            grid.stage(joined, ticks, &dc(per_block)),
            Staged::Placed { rounded_samples: 0 }
        );
        let block = grid.emit(51).expect("block with both slots");
        assert_eq!(block.time_ticks, ticks);
    }

    /// `(device rate, center, client rate, carrier, bandwidth, block that divides the ratio)`.
    const FRACTIONAL_PLANS: [(u64, u64, u64, u64, u64, usize); 4] = [
        (
            9_720_000,
            865_200_000,
            4_915_200,
            864_570_000,
            1_500_000,
            18_225,
        ),
        (
            12_288_000,
            866_300_000,
            800_000,
            860_850_000,
            200_000,
            23_040,
        ),
        (
            12_288_000,
            866_300_000,
            388_800,
            869_040_000,
            30_000,
            23_040,
        ),
        (3_888_000, 867_520_000, 388_800, 869_040_000, 30_000, 3_240),
    ];

    fn fractional_plan(
        rate: u64,
        center: u64,
        client: u64,
        carrier: u64,
        bandwidth: u64,
    ) -> SlicePlan {
        CompositeConfig {
            device_rate_hz: rate,
            tx_center_hz: center,
            ..PLAN
        }
        .plan_slice(Direction::Tx, client as f64, carrier, Some(bandwidth))
        .unwrap()
    }

    /// Nearest device tick of a client sample, with one tick per device sample.
    fn tick_of(plan: &SlicePlan, index: u64) -> u64 {
        ((index as u128 * plan.ratio as u128 + plan.denominator as u128 / 2)
            / plan.denominator as u128) as u64
    }

    fn client_stream(len: usize) -> Vec<Complex32> {
        (0..len)
            .map(|n| Complex32::new((n % 29) as f32 / 29.0, (n % 17) as f32 / 17.0 - 0.5))
            .collect()
    }

    /// Stages `input` from client sample `first` in uneven chunks and emits `blocks` blocks.
    fn run_grid(
        rate: u64,
        plan: &SlicePlan,
        block_samples: usize,
        first: u64,
        input: &[Complex32],
        blocks: u64,
    ) -> Vec<Complex32> {
        let mut grid = TxGrid::new(rate, rate, block_samples).unwrap();
        let slot = grid.add_slot(plan, rate, 1.0);
        let mut index = first;
        for chunk in input.chunks(997) {
            let staged = grid.stage(slot, tick_of(plan, index), chunk);
            assert!(matches!(staged, Staged::Placed { .. }), "{staged:?}");
            index += chunk.len() as u64;
        }
        let start = grid.next_block.unwrap();
        (start..start + blocks)
            .flat_map(|block| grid.emit(block).unwrap().samples)
            .collect()
    }

    #[test]
    fn a_block_that_splits_client_samples_gives_the_same_stream_as_one_that_does_not() {
        for (rate, center, client, carrier, bandwidth, whole) in FRACTIONAL_PLANS {
            let plan = fractional_plan(rate, center, client, carrier, bandwidth);
            let span = whole * 6;
            let input = client_stream(span * plan.denominator / plan.ratio + 1);
            let reference = run_grid(rate, &plan, whole, 0, &input, 6);
            for block in [whole + 1, whole - 7, rate as usize / 500, 4_099] {
                let blocks = span.div_ceil(block) as u64;
                let stream = run_grid(rate, &plan, block, 0, &input, blocks);
                assert_eq!(
                    stream[..span],
                    reference[..span],
                    "{client} Hz client with a {block}-sample block"
                );
            }
        }
    }

    #[test]
    fn a_block_a_client_skips_matches_one_it_filled_with_silence() {
        let (rate, center, client, carrier, bandwidth, whole) = FRACTIONAL_PLANS[1];
        let plan = fractional_plan(rate, center, client, carrier, bandwidth);
        let block = whole + 1;
        let run = |fill_gap: bool| {
            let mut grid = TxGrid::new(rate, rate, block).unwrap();
            let slot = grid.add_slot(&plan, rate, 1.0);
            let blocks = 4u64;
            for b in 0..blocks {
                let first = grid.slots[slot].first_input(b, block);
                let len = grid.slots[slot].inputs_in(b, block);
                let samples = if b != 2 {
                    dc(len)
                } else if fill_gap {
                    vec![Complex32::default(); len]
                } else {
                    continue;
                };
                grid.stage(slot, tick_of(&plan, first), &samples);
            }
            (0..blocks)
                .flat_map(|b| grid.emit(b).unwrap().samples)
                .collect::<Vec<_>>()
        };
        assert_eq!(run(false), run(true));
    }

    #[test]
    fn a_slot_starting_mid_stream_on_a_split_block_keeps_every_sample() {
        for (rate, center, client, carrier, bandwidth, whole) in FRACTIONAL_PLANS {
            let plan = fractional_plan(rate, center, client, carrier, bandwidth);
            let block = whole + 1;
            let start_block = 5u64;
            let mut grid = TxGrid::new(rate, rate, block).unwrap();
            let slot = grid.add_slot(&plan, rate, 1.0);
            let first = grid.slots[slot].first_input(start_block, block);
            let tick = tick_of(&plan, first);
            let len = (block * 4 * plan.denominator).div_ceil(plan.ratio) + 1;
            assert!(matches!(
                grid.stage(slot, tick, &dc(len)),
                Staged::Placed { .. }
            ));
            assert_eq!(grid.next_block, Some(start_block));
            let stream: Vec<Complex32> = (start_block..start_block + 4)
                .flat_map(|b| grid.emit(b).unwrap().samples)
                .collect();
            // Past the filter transient, DC comes out at unit level on the carrier.
            for (n, sample) in stream.iter().enumerate().skip(block) {
                assert!(
                    (sample.norm() - 1.0).abs() < 0.01,
                    "{client} Hz sample {n} is {}",
                    sample.norm()
                );
            }
        }
    }
}
