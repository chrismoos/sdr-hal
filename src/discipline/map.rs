//! Map between a device clock and a clock disciplined to an external reference.
//!
//! The map is piecewise linear and continuous, so the disciplined clock never steps.
//! One writer publishes segments and any number of readers copy them without locking.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const SEGMENTS: usize = 4;
const WORDS_PER_SEGMENT: usize = 4;
const MAP_WORDS: usize = 1 + SEGMENTS * WORDS_PER_SEGMENT;

/// A tick count with a fraction in `[0, 1)`, so a position keeps sub-tick precision at
/// any distance from zero.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Position {
    pub whole: u64,
    pub frac: f64,
}

impl Position {
    pub(crate) fn from_ticks(ticks: u64) -> Self {
        Self {
            whole: ticks,
            frac: 0.0,
        }
    }

    pub(crate) fn offset(self, ticks: f64) -> Self {
        let total = self.frac + ticks;
        let carry = total.floor();
        Self {
            whole: self.whole.wrapping_add_signed(carry as i64),
            frac: total - carry,
        }
    }

    /// `self - other` in ticks.
    pub(crate) fn minus(self, other: Self) -> f64 {
        (self.whole as i64).wrapping_sub(other.whole as i64) as f64 + (self.frac - other.frac)
    }

    pub(crate) fn ceil(self) -> u64 {
        if self.frac > 0.0 {
            self.whole + 1
        } else {
            self.whole
        }
    }

    pub(crate) fn round(self) -> u64 {
        if self.frac >= 0.5 {
            self.whole + 1
        } else {
            self.whole
        }
    }
}

/// One linear piece of the map, in effect from `device_start` on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Segment {
    pub device_start: u64,
    pub disciplined_start: Position,
    /// Device ticks per reference tick, minus one.
    pub rate_error: f64,
}

impl Segment {
    fn disciplined_at(&self, device: Position) -> Position {
        let delta = device.minus(Position::from_ticks(self.device_start));
        let shrink = delta * self.rate_error / (1.0 + self.rate_error);
        self.disciplined_start
            .offset((device.whole as i64).wrapping_sub(self.device_start as i64) as f64)
            .offset(device.frac - shrink)
    }

    fn device_at(&self, disciplined: Position) -> Position {
        let delta = disciplined.minus(self.disciplined_start);
        Position::from_ticks(self.device_start)
            .offset(
                (disciplined.whole as i64).wrapping_sub(self.disciplined_start.whole as i64) as f64,
            )
            .offset(disciplined.frac - self.disciplined_start.frac + delta * self.rate_error)
    }
}

/// Words written under a sequence counter. A reader retries while a write is in
/// progress, so it never returns a torn value and never blocks the writer.
pub(crate) struct SeqWords<const N: usize> {
    sequence: AtomicU64,
    words: [AtomicU64; N],
}

impl<const N: usize> SeqWords<N> {
    pub(crate) fn new() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            words: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// Only one thread may write.
    pub(crate) fn write(&self, words: &[u64; N]) {
        self.sequence.fetch_add(1, Ordering::AcqRel);
        for (slot, word) in self.words.iter().zip(words) {
            slot.store(*word, Ordering::Release);
        }
        self.sequence.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn sequence(&self) -> u64 {
        self.sequence.load(Ordering::Acquire)
    }

    pub(crate) fn read(&self) -> (u64, [u64; N]) {
        loop {
            let before = self.sequence.load(Ordering::Acquire);
            if before % 2 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let words = std::array::from_fn(|index| self.words[index].load(Ordering::Acquire));
            if self.sequence.load(Ordering::Acquire) == before {
                return (before, words);
            }
        }
    }
}

/// The published map. An empty map is the identity.
pub(crate) struct SharedMap {
    words: SeqWords<MAP_WORDS>,
}

impl SharedMap {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            words: SeqWords::new(),
        })
    }

    /// Appends a segment, dropping the oldest when the map is full. Only one thread
    /// may publish.
    pub(crate) fn publish(&self, segment: Segment) {
        let current = self.snapshot();
        let kept = current.segments();
        let kept = &kept[usize::from(kept.len() == SEGMENTS)..];
        let mut words = [0u64; MAP_WORDS];
        words[0] = kept.len() as u64 + 1;
        for (index, segment) in kept.iter().chain(std::iter::once(&segment)).enumerate() {
            let base = 1 + index * WORDS_PER_SEGMENT;
            words[base] = segment.device_start;
            words[base + 1] = segment.disciplined_start.whole;
            words[base + 2] = segment.disciplined_start.frac.to_bits();
            words[base + 3] = segment.rate_error.to_bits();
        }
        self.words.write(&words);
    }

    pub(crate) fn snapshot(&self) -> Snapshot {
        decode(&self.words.read().1)
    }
}

fn decode(words: &[u64; MAP_WORDS]) -> Snapshot {
    let empty = Segment {
        device_start: 0,
        disciplined_start: Position::from_ticks(0),
        rate_error: 0.0,
    };
    let mut segments = [empty; SEGMENTS];
    let len = (words[0] as usize).min(SEGMENTS);
    for (index, segment) in segments.iter_mut().enumerate().take(len) {
        let base = 1 + index * WORDS_PER_SEGMENT;
        *segment = Segment {
            device_start: words[base],
            disciplined_start: Position {
                whole: words[base + 1],
                frac: f64::from_bits(words[base + 2]),
            },
            rate_error: f64::from_bits(words[base + 3]),
        };
    }
    Snapshot { segments, len }
}

#[derive(Clone, Copy)]
pub(crate) struct Snapshot {
    segments: [Segment; SEGMENTS],
    len: usize,
}

/// A reader's copy of the map, refreshed when the published map changes.
pub(crate) struct MapView {
    shared: Arc<SharedMap>,
    seen: u64,
    snapshot: Snapshot,
}

impl MapView {
    pub(crate) fn new(shared: Arc<SharedMap>) -> Self {
        let (seen, words) = shared.words.read();
        Self {
            shared,
            seen,
            snapshot: decode(&words),
        }
    }

    pub(crate) fn refresh(&mut self) {
        if self.shared.words.sequence() != self.seen {
            let (seen, words) = self.shared.words.read();
            self.seen = seen;
            self.snapshot = decode(&words);
        }
    }
}

impl std::ops::Deref for MapView {
    type Target = Snapshot;

    fn deref(&self) -> &Snapshot {
        &self.snapshot
    }
}

impl Snapshot {
    fn segments(&self) -> &[Segment] {
        &self.segments[..self.len]
    }

    fn for_device(&self, device: u64) -> Option<&Segment> {
        self.segments()
            .iter()
            .rev()
            .find(|segment| segment.device_start <= device)
            .or(self.segments().first())
    }

    fn for_disciplined(&self, disciplined: Position) -> Option<&Segment> {
        self.segments()
            .iter()
            .rev()
            .find(|segment| disciplined.minus(segment.disciplined_start) >= 0.0)
            .or(self.segments().first())
    }

    pub(crate) fn disciplined_at(&self, device: Position) -> Position {
        match self.for_device(device.whole) {
            Some(segment) if segment.device_start <= device.whole => segment.disciplined_at(device),
            // Before the first segment the two clocks run at the same rate.
            Some(first) => first
                .disciplined_start
                .offset(device.minus(Position::from_ticks(first.device_start))),
            None => device,
        }
    }

    pub(crate) fn device_at(&self, disciplined: Position) -> Position {
        match self.for_disciplined(disciplined) {
            Some(segment) if disciplined.minus(segment.disciplined_start) >= 0.0 => {
                segment.device_at(disciplined)
            }
            Some(first) => Position::from_ticks(first.device_start)
                .offset(disciplined.minus(first.disciplined_start)),
            None => disciplined,
        }
    }

    /// Rate error in effect at `device`, and the device tick where the next segment starts.
    pub(crate) fn rate_at_device(&self, device: u64) -> (f64, Option<u64>) {
        let rate = match self.for_device(device) {
            Some(segment) if segment.device_start <= device => segment.rate_error,
            _ => 0.0,
        };
        let next = self
            .segments()
            .iter()
            .map(|segment| segment.device_start)
            .find(|start| *start > device);
        (rate, next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK_RATE: u64 = 30_720_000;
    const FAST: f64 = 1.3e-6;

    fn view_with(segments: &[Segment]) -> MapView {
        let shared = SharedMap::new();
        for segment in segments {
            shared.publish(*segment);
        }
        MapView::new(shared)
    }

    #[test]
    fn an_empty_map_is_the_identity() {
        let view = view_with(&[]);
        let position = Position {
            whole: 123_456,
            frac: 0.25,
        };
        assert_eq!(view.disciplined_at(position), position);
        assert_eq!(view.device_at(position), position);
    }

    #[test]
    fn a_fast_device_clock_maps_to_fewer_disciplined_ticks() {
        let start = 10 * TICK_RATE;
        let view = view_with(&[Segment {
            device_start: start,
            disciplined_start: Position::from_ticks(start),
            rate_error: FAST,
        }]);
        // One reference second later the device has counted `1 + FAST` seconds of ticks.
        let device = Position::from_ticks(start).offset(TICK_RATE as f64 * (1.0 + FAST));
        let disciplined = view.disciplined_at(device);
        let elapsed = disciplined.minus(Position::from_ticks(start));
        assert!(
            (elapsed - TICK_RATE as f64).abs() < 1e-6,
            "elapsed={elapsed}"
        );
    }

    #[test]
    fn the_two_directions_invert_each_other_far_from_zero() {
        let start = 400 * 86_400 * TICK_RATE;
        let view = view_with(&[Segment {
            device_start: start,
            disciplined_start: Position {
                whole: start - 77,
                frac: 0.375,
            },
            rate_error: -FAST,
        }]);
        let device = Position {
            whole: start + 5 * TICK_RATE + 11,
            frac: 0.6,
        };
        let back = view.device_at(view.disciplined_at(device));
        assert!(
            back.minus(device).abs() < 1e-6,
            "error={}",
            back.minus(device)
        );
    }

    #[test]
    fn the_map_is_continuous_across_a_rate_change() {
        let first = Segment {
            device_start: TICK_RATE,
            disciplined_start: Position::from_ticks(TICK_RATE),
            rate_error: FAST,
        };
        let boundary = 2 * TICK_RATE;
        let mut view = view_with(&[first]);
        let second = Segment {
            device_start: boundary,
            disciplined_start: view.disciplined_at(Position::from_ticks(boundary)),
            rate_error: FAST / 2.0,
        };
        view.shared.publish(second);
        view.refresh();
        let before = view.disciplined_at(Position::from_ticks(boundary - 1));
        let after = view.disciplined_at(Position::from_ticks(boundary));
        let step = after.minus(before);
        assert!((step - 1.0).abs() < 1e-5, "step={step}");
        assert_eq!(view.rate_at_device(boundary - 1), (FAST, Some(boundary)));
        assert_eq!(view.rate_at_device(boundary), (FAST / 2.0, None));
    }

    #[test]
    fn a_full_map_drops_its_oldest_segment() {
        let shared = SharedMap::new();
        for index in 0..(SEGMENTS as u64 + 2) {
            shared.publish(Segment {
                device_start: index * TICK_RATE,
                disciplined_start: Position::from_ticks(index * TICK_RATE),
                rate_error: 0.0,
            });
        }
        let view = MapView::new(shared);
        assert_eq!(view.segments().len(), SEGMENTS);
        assert_eq!(view.segments()[0].device_start, 2 * TICK_RATE);
    }
}
