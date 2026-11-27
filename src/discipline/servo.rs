//! Steers the clock map so each reference pulse lands a whole second after the last.

use std::sync::Arc;

use super::map::{MapView, Position, Segment, SeqWords, SharedMap};
use crate::{ClockStatus, ReferenceLock, ReferenceSource, TimeReference};

/// 1980-01-06T00:00:00Z as Unix seconds.
const REFERENCE_EPOCH_UNIX_SECONDS: u64 = 315_964_800;
const NANOS_PER_SECOND: f64 = 1e9;
const PARTS_PER_BILLION: f64 = 1e9;

/// Consecutive consistent pulse periods measured before the rate is trusted.
const ACQUIRE_PERIODS: usize = 4;
/// A pulse period further than this from one second is not a reference pulse.
const MAX_PERIOD_ERROR: f64 = 50e-6;
/// Pulses that may go missing between two accepted ones.
const MAX_MISSED_PULSES: f64 = 5.0;
/// Proportional and integral loop gains for a pulse every second, critically damped.
#[derive(Clone, Copy)]
struct Gains {
    proportional: f64,
    integral: f64,
}
/// Settles in about 20 s and follows an oscillator that is still warming.
const FAST_GAINS: Gains = Gains {
    proportional: 0.1,
    integral: 0.0025,
};
/// Averages receiver pulse jitter over about a minute. The proportional gain is the
/// share of each pulse's jitter that reaches the carrier as a frequency offset.
const SLOW_GAINS: Gains = Gains {
    proportional: 0.03,
    integral: 0.000_225,
};
/// Weight of one pulse in the running mean of the pulse offset.
const MEAN_WEIGHT: f64 = 1.0 / 16.0;
/// The slow gains apply once the mean offset has stayed inside this for
/// `CALM_PULSES` pulses.
const CALM_NS: f64 = 50.0;
const CALM_PULSES: u32 = 30;
/// A mean offset beyond this means the oscillator moves faster than the slow gains
/// follow, so the fast gains apply again.
const RESTLESS_NS: f64 = 250.0;
/// Largest change of the learned rate per pulse, so one bad measurement moves it little.
const MAX_INTEGRAL_STEP: f64 = 5e-8;
/// Largest rate offset applied to pull phase in, which bounds the carrier shift it causes.
const MAX_PHASE_PULL: f64 = 2e-6;
/// A pulse further than this from its expected time is rejected.
const REJECT_NS: f64 = 5_000.0;
/// Rejected pulses in a row that mean the reference moved, so tracking starts over.
const REJECT_LIMIT: u32 = 5;
/// Seconds without an accepted pulse before the clock is in holdover.
const HOLDOVER_AFTER_S: f64 = 2.5;
/// Bound on how fast the free-running oscillator rate moves in holdover, in parts per
/// billion per second. A warming B200-series VCTCXO measures about 0.2.
const HOLDOVER_DRIFT_NS_PER_S: f64 = 1.0;
const PULSE_UNCERTAINTY_NS: f64 = 50.0;
/// How far past a pulse a new segment starts, as a fraction of a second. A stream
/// that works ahead of the device clock must see the segment before it gets there.
const SEGMENT_LEAD: f64 = 0.5;
const MIN_SEGMENT_LEAD: f64 = 0.2;
/// Agreeing receiver times needed before the second count is trusted, and to replace
/// a second count that later reports disagree with.
const LABELS_REQUIRED: u32 = 3;

// Index of each value in the status words.
const LOCK: usize = 0;
const RATE: usize = 1;
const PHASE_ERROR: usize = 2;
const ACCEPTED: usize = 3;
const REJECTED: usize = 4;
const REFERENCE_TICKS: usize = 5;
const REFERENCE_SECONDS: usize = 6;
const REFERENCE_SUBSECOND: usize = 7;
const UNCERTAINTY: usize = 8;
const RECEIVER_FIX: usize = 9;
const SATELLITES: usize = 10;
const PRESENT: usize = 11;
const REPORT_TIME: usize = 12;
const LATITUDE: usize = 13;
const LONGITUDE: usize = 14;
const ALTITUDE: usize = 15;
const HDOP: usize = 16;
const SETTLED: usize = 17;
const STABILITY: usize = 18;
const STATUS_WORDS: usize = STABILITY + 1;
const HAS_STABILITY: u64 = 1 << 4;
/// Applied rates kept for the stability estimate, one per pulse.
const STABILITY_WINDOW: usize = 60;
const STABILITY_MIN_RATES: usize = 20;
/// Without correction, an oscillator measured further than this from the reference
/// is not locked to it, and its timestamps drift from the pulses. A GPS-disciplined
/// oscillator stays within a few ppb, and a free-running one is typically hundreds
/// of ppb off.
const UNCORRECTED_RATE_LIMIT: f64 = 50e-9;
/// Bits of the `PRESENT` word, one per optional value.
const HAS_REPORT_TIME: u64 = 1;
const HAS_POSITION: u64 = 1 << 1;
const HAS_ALTITUDE: u64 = 1 << 2;
const HAS_HDOP: u64 = 1 << 3;

/// Status and time reference shared with the stream halves.
pub(crate) struct SharedStatus {
    words: SeqWords<STATUS_WORDS>,
}

impl SharedStatus {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            words: SeqWords::new(),
        })
    }

    pub(crate) fn status(&self) -> ClockStatus {
        let (_, words) = self.words.read();
        ClockStatus {
            lock: lock_from_word(words[LOCK]),
            rate_error_ppb: f64::from_bits(words[RATE]),
            phase_error_ns: f64::from_bits(words[PHASE_ERROR]),
            accepted_pulses: words[ACCEPTED],
            rejected_pulses: words[REJECTED],
            receiver_fix: words[RECEIVER_FIX] != 0,
            // Stored one above the count, so zero stands for no report.
            satellites: words[SATELLITES].checked_sub(1).map(|count| count as u8),
            receiver_unix_seconds: (words[PRESENT] & HAS_REPORT_TIME != 0)
                .then(|| f64::from_bits(words[REPORT_TIME])),
            position: (words[PRESENT] & HAS_POSITION != 0).then(|| crate::ReceiverPosition {
                latitude_deg: f64::from_bits(words[LATITUDE]),
                longitude_deg: f64::from_bits(words[LONGITUDE]),
                altitude_m: (words[PRESENT] & HAS_ALTITUDE != 0)
                    .then(|| f64::from_bits(words[ALTITUDE])),
            }),
            hdop: (words[PRESENT] & HAS_HDOP != 0).then(|| f64::from_bits(words[HDOP])),
            settled: words[SETTLED] != 0,
            rate_stability_ppb: (words[PRESENT] & HAS_STABILITY != 0)
                .then(|| f64::from_bits(words[STABILITY])),
        }
    }

    pub(crate) fn time_reference(&self) -> TimeReference {
        let (_, words) = self.words.read();
        TimeReference {
            device_ticks: words[REFERENCE_TICKS],
            seconds: words[REFERENCE_SECONDS],
            subsecond_ns: words[REFERENCE_SUBSECOND] as u32,
            leap_seconds: 0,
            source: ReferenceSource::Gps,
            lock: lock_from_word(words[LOCK]),
            uncertainty_ns: words[UNCERTAINTY],
        }
    }
}

fn lock_word(lock: ReferenceLock) -> u64 {
    match lock {
        ReferenceLock::Unlocked => 0,
        ReferenceLock::Locked => 1,
        ReferenceLock::Holdover => 2,
    }
}

fn lock_from_word(word: u64) -> ReferenceLock {
    match word {
        1 => ReferenceLock::Locked,
        2 => ReferenceLock::Holdover,
        _ => ReferenceLock::Unlocked,
    }
}

enum Phase {
    /// Measuring the pulse period. Holds the device ticks of consecutive pulses.
    Acquiring(Vec<u64>),
    /// The rate is applied and the next pulse sets the phase the clock is held to.
    Settling,
    Tracking {
        /// Disciplined time of a pulse. Every later pulse belongs a whole number of
        /// seconds after it.
        reference: Position,
    },
}

pub(crate) struct Servo {
    tick_rate: u64,
    map: Arc<SharedMap>,
    view: MapView,
    status: Arc<SharedStatus>,
    phase: Phase,
    /// Learned oscillator rate error.
    rate: f64,
    /// Rate of the segment in use, which adds the pull toward the last pulse.
    applied_rate: f64,
    last_pulse: Option<u64>,
    last_accepted: Option<u64>,
    rejected_run: u32,
    accepted: u64,
    rejected: u64,
    phase_error_ns: f64,
    mean_error_ns: f64,
    calm_run: u32,
    slow: bool,
    holdover: bool,
    /// Unix seconds at the tracking reference pulse, with how many receiver times agreed.
    label: Option<(u64, u32)>,
    /// A different second count that recent receiver times agree on.
    challenger: Option<(u64, u32)>,
    receiver_locked: bool,
    satellites: Option<u8>,
    receiver_unix_seconds: Option<f64>,
    position: Option<crate::ReceiverPosition>,
    hdop: Option<f64>,
    /// The most recent applied rates, oldest first.
    applied_rates: std::collections::VecDeque<f64>,
    /// Whether the map follows the measured rate. Without it the map stays the
    /// identity and the pulses only measure the oscillator.
    correct: bool,
    /// The most recent pulse offsets in seconds, oldest first, while uncorrected.
    phase_errors: std::collections::VecDeque<f64>,
}

/// RMS distance of `rates` from their least-squares line.
fn spread_around_trend(rates: &std::collections::VecDeque<f64>) -> f64 {
    fit_line(rates).1
}

/// Least-squares slope of `values` per index, and their RMS distance from the line.
fn fit_line(rates: &std::collections::VecDeque<f64>) -> (f64, f64) {
    let count = rates.len() as f64;
    let mean_index = (count - 1.0) / 2.0;
    let mean_rate = rates.iter().sum::<f64>() / count;
    let (mut covariance, mut index_variance) = (0.0, 0.0);
    for (index, rate) in rates.iter().enumerate() {
        let offset = index as f64 - mean_index;
        covariance += offset * (rate - mean_rate);
        index_variance += offset * offset;
    }
    let slope = covariance / index_variance;
    let squares: f64 = rates
        .iter()
        .enumerate()
        .map(|(index, rate)| {
            let residual = rate - mean_rate - slope * (index as f64 - mean_index);
            residual * residual
        })
        .sum();
    (slope, (squares / count).sqrt())
}

impl Servo {
    pub(crate) fn new(
        tick_rate: u64,
        map: Arc<SharedMap>,
        status: Arc<SharedStatus>,
        correct: bool,
    ) -> Self {
        let view = MapView::new(Arc::clone(&map));
        Self {
            tick_rate,
            map,
            view,
            status,
            phase: Phase::Acquiring(Vec::new()),
            rate: 0.0,
            applied_rate: 0.0,
            last_pulse: None,
            last_accepted: None,
            rejected_run: 0,
            accepted: 0,
            rejected: 0,
            phase_error_ns: 0.0,
            mean_error_ns: 0.0,
            calm_run: 0,
            slow: false,
            holdover: false,
            label: None,
            challenger: None,
            receiver_locked: false,
            satellites: None,
            receiver_unix_seconds: None,
            position: None,
            hdop: None,
            applied_rates: std::collections::VecDeque::with_capacity(STABILITY_WINDOW),
            correct,
            phase_errors: std::collections::VecDeque::with_capacity(STABILITY_WINDOW),
        }
    }

    pub(crate) fn set_receiver(&mut self, report: &super::ReceiverReport) {
        self.receiver_locked = report.locked;
        self.satellites = report.satellites;
        self.receiver_unix_seconds = report.unix_seconds;
        self.hdop = report.hdop;
        // The last position stays reported after the fix is lost.
        if report.position.is_some() {
            self.position = report.position;
        }
    }

    fn ticks(&self) -> f64 {
        self.tick_rate as f64
    }

    /// Whether `period` device ticks is a whole number of pulse periods.
    fn plausible_period(&self, period: u64) -> bool {
        let seconds = period as f64 / self.ticks();
        let pulses = seconds.round();
        (1.0..=MAX_MISSED_PULSES + 1.0).contains(&pulses)
            && (seconds / pulses - 1.0).abs() <= MAX_PERIOD_ERROR
    }

    fn publish_segment(&mut self, pulse: u64, now: u64, rate_error: f64) {
        let lead = (self.ticks() * SEGMENT_LEAD) as u64;
        let earliest = now + (self.ticks() * MIN_SEGMENT_LEAD) as u64;
        let device_start = (pulse + lead).max(earliest);
        let disciplined_start = self.view.disciplined_at(Position::from_ticks(device_start));
        self.map.publish(Segment {
            device_start,
            disciplined_start,
            rate_error,
        });
        self.view.refresh();
    }

    fn restart(&mut self, pulse: Option<u64>) {
        self.phase = Phase::Acquiring(pulse.into_iter().collect());
        self.label = None;
        self.challenger = None;
        self.rejected_run = 0;
        self.holdover = false;
        self.mean_error_ns = 0.0;
        self.calm_run = 0;
        self.slow = false;
        self.applied_rates.clear();
        self.phase_errors.clear();
    }

    /// Measures the oscillator rate from how the pulse offsets drift, since nothing
    /// pulls them back.
    fn measure_uncorrected(&mut self, error_s: f64) {
        if self.phase_errors.len() == STABILITY_WINDOW {
            self.phase_errors.pop_front();
        }
        self.phase_errors.push_back(error_s);
        if self.phase_errors.len() >= STABILITY_MIN_RATES {
            // One pulse a second, so the drift per pulse is the rate error.
            self.rate = fit_line(&self.phase_errors).0;
        }
    }

    /// Selects the loop gains from how far the pulses sit from their expected times
    /// on average.
    fn gains(&mut self) -> Gains {
        self.mean_error_ns += MEAN_WEIGHT * (self.phase_error_ns - self.mean_error_ns);
        if self.mean_error_ns.abs() > RESTLESS_NS {
            self.slow = false;
            self.calm_run = 0;
        } else if self.mean_error_ns.abs() < CALM_NS {
            self.calm_run = self.calm_run.saturating_add(1);
            self.slow |= self.calm_run >= CALM_PULSES;
        } else {
            self.calm_run = 0;
        }
        if self.slow { SLOW_GAINS } else { FAST_GAINS }
    }

    /// Records a reference pulse latched at device tick `pulse`. `now` is the device
    /// clock at the time of the call.
    pub(crate) fn observe(&mut self, pulse: u64, now: u64) {
        let previous = self.last_pulse.replace(pulse);
        if !self.receiver_locked {
            return;
        }
        let plausible = previous
            .is_some_and(|previous| pulse > previous && self.plausible_period(pulse - previous));
        match &mut self.phase {
            Phase::Acquiring(pulses) => {
                if !plausible {
                    pulses.clear();
                }
                pulses.push(pulse);
                if pulses.len() > ACQUIRE_PERIODS {
                    let span = (pulse - pulses[0]) as f64 / self.tick_rate as f64;
                    self.rate = span / span.round() - 1.0;
                    if self.correct {
                        self.publish_segment(pulse, now, self.rate);
                    }
                    self.phase = Phase::Settling;
                }
            }
            Phase::Settling => {
                if !plausible {
                    self.restart(Some(pulse));
                    return;
                }
                let reference = self.view.disciplined_at(Position::from_ticks(pulse));
                self.phase = Phase::Tracking { reference };
                self.last_accepted = Some(pulse);
                self.accepted = 1;
                self.phase_error_ns = 0.0;
            }
            Phase::Tracking { reference } => {
                let reference = *reference;
                let offset = self
                    .view
                    .disciplined_at(Position::from_ticks(pulse))
                    .minus(reference);
                let error_ticks = offset - (offset / self.ticks()).round() * self.ticks();
                let error_s = error_ticks / self.ticks();
                if !plausible || (error_s * NANOS_PER_SECOND).abs() > REJECT_NS {
                    self.rejected += 1;
                    self.rejected_run += 1;
                    if self.rejected_run >= REJECT_LIMIT {
                        self.restart(Some(pulse));
                    }
                    return;
                }
                self.rejected_run = 0;
                self.holdover = false;
                self.accepted += 1;
                self.last_accepted = Some(pulse);
                self.phase_error_ns = error_s * NANOS_PER_SECOND;
                if !self.correct {
                    self.measure_uncorrected(error_s);
                    return;
                }
                let gains = self.gains();
                self.rate +=
                    (gains.integral * error_s).clamp(-MAX_INTEGRAL_STEP, MAX_INTEGRAL_STEP);
                let pull = (gains.proportional * error_s).clamp(-MAX_PHASE_PULL, MAX_PHASE_PULL);
                self.applied_rate = self.rate + pull;
                if self.applied_rates.len() == STABILITY_WINDOW {
                    self.applied_rates.pop_front();
                }
                self.applied_rates.push_back(self.applied_rate);
                log::debug!(
                    "clock: pulse offset_ns={:.1} mean_ns={:.1} rate_ppb={:.3} applied_ppb={:.3} slow={}",
                    self.phase_error_ns,
                    self.mean_error_ns,
                    self.rate * PARTS_PER_BILLION,
                    self.applied_rate * PARTS_PER_BILLION,
                    self.slow
                );
                self.publish_segment(pulse, now, self.rate + pull);
            }
        }
    }

    /// Records the receiver's Unix time for the pulse latched at device tick `pulse`.
    pub(crate) fn label(&mut self, pulse: u64, unix_seconds: u64) {
        let Phase::Tracking { reference } = &self.phase else {
            return;
        };
        let offset = self
            .view
            .disciplined_at(Position::from_ticks(pulse))
            .minus(*reference);
        let pulses = (offset / self.ticks()).round() as i64;
        let Some(at_reference) = unix_seconds.checked_add_signed(-pulses) else {
            return;
        };
        match self.label {
            Some((seconds, agreed)) if seconds == at_reference => {
                self.label = Some((seconds, agreed.saturating_add(1)));
                self.challenger = None;
            }
            // One report read late must not move a second count that many agreed on.
            Some((_, agreed)) if agreed >= LABELS_REQUIRED => {
                let count = match self.challenger {
                    Some((seconds, count)) if seconds == at_reference => count + 1,
                    _ => 1,
                };
                if count >= LABELS_REQUIRED {
                    self.label = Some((at_reference, count));
                    self.challenger = None;
                } else {
                    self.challenger = Some((at_reference, count));
                }
            }
            _ => self.label = Some((at_reference, 1)),
        }
    }

    /// Updates holdover from the device clock and publishes the status.
    pub(crate) fn poll(&mut self, now: u64) {
        if let (Phase::Tracking { .. }, Some(accepted)) = (&self.phase, self.last_accepted) {
            let silent_s = now.saturating_sub(accepted) as f64 / self.ticks();
            if silent_s > HOLDOVER_AFTER_S && !self.holdover {
                self.holdover = true;
                // The phase pull of the last pulse must not run on without pulses.
                if self.correct {
                    self.publish_segment(accepted, now, self.rate);
                }
            }
        }
        self.publish_status(now);
    }

    fn lock(&self) -> ReferenceLock {
        let labeled = self
            .label
            .is_some_and(|(_, agreed)| agreed >= LABELS_REQUIRED);
        // The rate measured while acquiring stands until enough offsets refine it, so a
        // drifting oscillator never reads as locked after tracking restarts.
        let drifting = !self.correct && self.rate.abs() > UNCORRECTED_RATE_LIMIT;
        match &self.phase {
            Phase::Tracking { .. } if drifting => ReferenceLock::Unlocked,
            Phase::Tracking { .. } if labeled && self.holdover => ReferenceLock::Holdover,
            Phase::Tracking { .. } if labeled => ReferenceLock::Locked,
            _ => ReferenceLock::Unlocked,
        }
    }

    fn publish_status(&mut self, now: u64) {
        let mut words = [0u64; STATUS_WORDS];
        words[LOCK] = lock_word(self.lock());
        words[RATE] = (self.rate * PARTS_PER_BILLION).to_bits();
        words[PHASE_ERROR] = self.phase_error_ns.to_bits();
        words[ACCEPTED] = self.accepted;
        words[REJECTED] = self.rejected;
        words[RECEIVER_FIX] = u64::from(self.receiver_locked);
        words[SATELLITES] = self.satellites.map_or(0, |count| u64::from(count) + 1);
        if let Some(seconds) = self.receiver_unix_seconds {
            words[PRESENT] |= HAS_REPORT_TIME;
            words[REPORT_TIME] = seconds.to_bits();
        }
        if let Some(position) = self.position {
            words[PRESENT] |= HAS_POSITION;
            words[LATITUDE] = position.latitude_deg.to_bits();
            words[LONGITUDE] = position.longitude_deg.to_bits();
            if let Some(altitude) = position.altitude_m {
                words[PRESENT] |= HAS_ALTITUDE;
                words[ALTITUDE] = altitude.to_bits();
            }
        }
        if let Some(hdop) = self.hdop {
            words[PRESENT] |= HAS_HDOP;
            words[HDOP] = hdop.to_bits();
        }
        words[SETTLED] = u64::from(self.slow);
        if self.applied_rates.len() >= STABILITY_MIN_RATES {
            words[PRESENT] |= HAS_STABILITY;
            words[STABILITY] =
                (spread_around_trend(&self.applied_rates) * PARTS_PER_BILLION).to_bits();
        }
        if let (Phase::Tracking { reference }, Some((unix_seconds, _)), Some(accepted)) =
            (&self.phase, self.label, self.last_accepted)
        {
            // The reference is reported at the latest accepted pulse, so a client
            // extrapolates over the shortest span.
            let offset = self
                .view
                .disciplined_at(Position::from_ticks(accepted))
                .minus(*reference);
            let pulses = (offset / self.ticks()).round();
            let at_pulse = reference.offset(pulses * self.ticks());
            let device_ticks = at_pulse.ceil();
            let past_pulse_s = Position::from_ticks(device_ticks).minus(at_pulse) / self.ticks();
            let silent_s = now.saturating_sub(accepted) as f64 / self.ticks();
            let holdover_ns = if self.holdover {
                // A rate drifting at a constant pace moves time by half the pace times the
                // square of the elapsed time.
                HOLDOVER_DRIFT_NS_PER_S * silent_s * silent_s / 2.0
            } else {
                0.0
            };
            words[REFERENCE_TICKS] = device_ticks;
            words[REFERENCE_SECONDS] = unix_seconds
                .wrapping_add_signed(pulses as i64)
                .saturating_sub(REFERENCE_EPOCH_UNIX_SECONDS);
            words[REFERENCE_SUBSECOND] = (past_pulse_s * NANOS_PER_SECOND).round() as u64;
            words[UNCERTAINTY] =
                (self.phase_error_ns.abs() + PULSE_UNCERTAINTY_NS + holdover_ns) as u64;
        }
        self.status.words.write(&words);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK_RATE: u64 = 30_720_000;
    const UNIX_START: u64 = 1_795_600_000;

    fn report_with_fix(locked: bool) -> super::super::ReceiverReport {
        super::super::ReceiverReport {
            unix_seconds: Some(UNIX_START as f64),
            locked,
            satellites: Some(if locked { 7 } else { 2 }),
            position: locked.then_some(crate::ReceiverPosition {
                latitude_deg: 33.3393,
                longitude_deg: -111.7161,
                altitude_m: Some(412.1),
            }),
            hdop: locked.then_some(1.1),
        }
    }

    /// A device oscillator that runs `rate_error` fast, with a reference pulse every second.
    struct Bench {
        servo: Servo,
        status: Arc<SharedStatus>,
        view: MapView,
        rate_error: f64,
        /// Change of the oscillator rate error per second, from second `drift_from`.
        drift: f64,
        drift_from: u64,
        /// Peak of the uniform error on each pulse's arrival time.
        jitter_ns: f64,
        /// Device tick of the pulse at reference second zero.
        first_pulse: f64,
        second: u64,
    }

    impl Bench {
        fn new(rate_error: f64) -> Self {
            Self::with_correction(rate_error, true)
        }

        fn with_correction(rate_error: f64, correct: bool) -> Self {
            let map = SharedMap::new();
            let status = SharedStatus::new();
            let mut servo = Servo::new(TICK_RATE, Arc::clone(&map), Arc::clone(&status), correct);
            servo.set_receiver(&report_with_fix(true));
            Self {
                servo,
                status,
                view: MapView::new(map),
                rate_error,
                drift: 0.0,
                drift_from: 0,
                jitter_ns: 0.0,
                first_pulse: 1_234_567.0,
                second: 0,
            }
        }

        fn pulse_ticks(&self, second: u64) -> u64 {
            let seconds = second as f64;
            let drifting = second.saturating_sub(self.drift_from) as f64;
            let elapsed =
                seconds * (1.0 + self.rate_error) + self.drift * drifting * drifting / 2.0;
            // A fixed hash of the second, so every run sees the same jitter.
            let hash = second.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 11;
            let jitter = (hash as f64 / (1u64 << 53) as f64 * 2.0 - 1.0) * self.jitter_ns;
            (self.first_pulse + (elapsed + jitter / NANOS_PER_SECOND) * TICK_RATE as f64).round()
                as u64
        }

        /// Delivers the next pulse the way the runner does: observed a little late,
        /// labeled, then polled.
        fn step(&mut self) {
            let pulse = self.pulse_ticks(self.second);
            let now = pulse + TICK_RATE / 10;
            self.servo.observe(pulse, now);
            self.servo.label(pulse, UNIX_START + self.second);
            self.servo.poll(now);
            self.view.refresh();
            self.second += 1;
        }

        fn skip(&mut self, seconds: u64) {
            self.second += seconds;
            let now = self.pulse_ticks(self.second) - TICK_RATE / 2;
            self.servo.poll(now);
        }

        /// Disciplined time of the pulse at `second`, in seconds past the pulse the
        /// reference reports.
        fn pulse_phase_ns(&self, second: u64) -> f64 {
            let reference = self.status.time_reference();
            let disciplined = self
                .view
                .disciplined_at(Position::from_ticks(self.pulse_ticks(second)));
            let offset = disciplined.minus(Position::from_ticks(reference.device_ticks))
                / TICK_RATE as f64
                + f64::from(reference.subsecond_ns) / NANOS_PER_SECOND;
            (offset - offset.round()) * NANOS_PER_SECOND
        }
    }

    #[test]
    fn the_status_reports_how_steady_the_applied_rate_is() {
        let mut bench = Bench::new(-0.22e-6);
        bench.jitter_ns = 50.0;
        for _ in 0..15 {
            bench.step();
        }
        assert_eq!(bench.status.status().rate_stability_ppb, None);
        for _ in 0..250 {
            bench.step();
        }
        let settled = bench.status.status().rate_stability_ppb.unwrap();
        assert!(settled > 0.05 && settled < 2.0, "settled={settled} ppb");

        let mut quiet = Bench::new(-0.22e-6);
        for _ in 0..250 {
            quiet.step();
        }
        let steady = quiet.status.status().rate_stability_ppb.unwrap();
        assert!(steady < settled / 2.0, "steady={steady} settled={settled}");
    }

    #[test]
    fn the_status_keeps_the_last_position_after_the_fix_is_lost() {
        let mut bench = Bench::new(0.3e-6);
        for _ in 0..10 {
            bench.step();
        }
        let status = bench.status.status();
        assert!(status.receiver_fix);
        assert_eq!(status.satellites, Some(7));
        assert_eq!(status.hdop, Some(1.1));
        assert_eq!(status.receiver_unix_seconds, Some(UNIX_START as f64));
        let position = status.position.unwrap();
        assert_eq!(position.latitude_deg, 33.3393);
        assert_eq!(position.altitude_m, Some(412.1));

        bench.servo.set_receiver(&report_with_fix(false));
        bench.step();
        let status = bench.status.status();
        assert!(!status.receiver_fix);
        assert_eq!(status.satellites, Some(2));
        assert_eq!(status.hdop, None);
        assert_eq!(status.position, Some(position));
    }

    #[test]
    fn without_correction_a_locked_oscillator_keeps_the_map_as_the_identity() {
        let mut bench = Bench::with_correction(0.0, false);
        bench.jitter_ns = 40.0;
        for _ in 0..60 {
            bench.step();
        }
        let status = bench.status.status();
        assert_eq!(status.lock, ReferenceLock::Locked);
        assert!(status.rate_error_ppb.abs() < 5.0, "{status:?}");
        for device in [0u64, 123_456_789, 9_876_543_210] {
            let mapped = bench.view.disciplined_at(Position::from_ticks(device));
            assert_eq!(mapped.minus(Position::from_ticks(device)), 0.0);
        }
        let phase = bench.pulse_phase_ns(bench.second);
        assert!(phase.abs() < 100.0, "phase={phase} ns");
    }

    #[test]
    fn without_correction_a_free_running_oscillator_is_not_locked() {
        let mut bench = Bench::with_correction(-0.22e-6, false);
        for _ in 0..40 {
            bench.step();
        }
        let status = bench.status.status();
        assert_eq!(status.lock, ReferenceLock::Unlocked);
        assert!((status.rate_error_ppb + 220.0).abs() < 5.0, "{status:?}");
    }

    #[test]
    fn a_fast_oscillator_is_measured_and_locked() {
        let mut bench = Bench::new(1.3e-6);
        for _ in 0..40 {
            bench.step();
        }
        let status = bench.status.status();
        assert_eq!(status.lock, ReferenceLock::Locked);
        assert!((status.rate_error_ppb - 1300.0).abs() < 20.0, "{status:?}");
        assert!(status.phase_error_ns.abs() < 100.0, "{status:?}");
        assert_eq!(status.rejected_pulses, 0);
    }

    /// Largest swing of the applied rate over the last `pulses` pulses, in ppb.
    fn applied_rate_swing_ppb(bench: &mut Bench, pulses: usize) -> f64 {
        let (mut low, mut high) = (f64::MAX, f64::MIN);
        for _ in 0..pulses {
            bench.step();
            let rate = (bench.servo.applied_rate - bench.rate_error) * PARTS_PER_BILLION;
            low = low.min(rate);
            high = high.max(rate);
        }
        high - low
    }

    #[test]
    fn a_settled_clock_passes_less_pulse_jitter_to_the_carrier() {
        let mut bench = Bench::new(-0.22e-6);
        bench.jitter_ns = 50.0;
        for _ in 0..25 {
            bench.step();
        }
        assert!(!bench.servo.slow);
        let settling = applied_rate_swing_ppb(&mut bench, 20);
        for _ in 0..120 {
            bench.step();
        }
        assert!(bench.servo.slow);
        let settled = applied_rate_swing_ppb(&mut bench, 200);
        assert!(settled < 4.5, "settled swing {settled} ppb");
        assert!(
            settling > 2.0 * settled,
            "{settling} ppb against {settled} ppb"
        );
        let phase = bench.status.status().phase_error_ns;
        assert!(phase.abs() < 150.0, "phase={phase} ns");
    }

    #[test]
    fn a_drifting_oscillator_brings_the_fast_gains_back() {
        let mut bench = Bench::new(0.4e-6);
        for _ in 0..150 {
            bench.step();
        }
        assert!(bench.servo.slow);
        // 0.5 ppb per second, faster than the slow gains follow.
        bench.drift = 0.5e-9;
        bench.drift_from = bench.second;
        let mut worst = 0.0f64;
        for _ in 0..300 {
            bench.step();
            worst = worst.max(bench.status.status().phase_error_ns.abs());
        }
        assert!(!bench.servo.slow);
        assert!(worst < 600.0, "worst phase {worst} ns");
        assert_eq!(bench.status.status().rejected_pulses, 0);
    }

    #[test]
    fn a_locked_clock_keeps_later_pulses_on_whole_seconds() {
        for rate_error in [-0.7e-6, 0.0, 2.2e-6] {
            let mut bench = Bench::new(rate_error);
            for _ in 0..60 {
                bench.step();
            }
            let phase = bench.pulse_phase_ns(bench.second);
            assert!(phase.abs() < 100.0, "rate={rate_error} phase={phase} ns");
        }
    }

    #[test]
    fn the_reference_reports_the_receiver_second_of_a_pulse() {
        let mut bench = Bench::new(0.4e-6);
        for _ in 0..20 {
            bench.step();
        }
        let reference = bench.status.time_reference();
        assert_eq!(reference.source, ReferenceSource::Gps);
        assert_eq!(reference.leap_seconds, 0);
        let last = bench.second - 1;
        assert_eq!(
            reference.seconds,
            UNIX_START + last - REFERENCE_EPOCH_UNIX_SECONDS
        );
        let pulse = bench
            .view
            .disciplined_at(Position::from_ticks(bench.pulse_ticks(last)));
        let ahead = Position::from_ticks(reference.device_ticks).minus(pulse);
        assert!((0.0..1.0).contains(&ahead), "ahead={ahead}");
    }

    #[test]
    fn the_clock_is_unlocked_until_enough_receiver_times_agree() {
        let mut bench = Bench::new(0.0);
        for _ in 0..(ACQUIRE_PERIODS + 3) {
            bench.step();
        }
        assert_eq!(bench.status.status().lock, ReferenceLock::Unlocked);
        bench.step();
        bench.step();
        assert_eq!(bench.status.status().lock, ReferenceLock::Locked);
    }

    #[test]
    fn one_stray_receiver_time_does_not_move_the_second_count() {
        let mut bench = Bench::new(0.3e-6);
        for _ in 0..20 {
            bench.step();
        }
        let pulse = bench.pulse_ticks(bench.second - 1);
        bench.servo.label(pulse, UNIX_START + bench.second - 2);
        bench.servo.poll(pulse + TICK_RATE / 10);
        let reference = bench.status.time_reference();
        assert_eq!(reference.lock, ReferenceLock::Locked);
        assert_eq!(
            reference.seconds,
            UNIX_START + bench.second - 1 - REFERENCE_EPOCH_UNIX_SECONDS
        );
    }

    #[test]
    fn receiver_times_that_keep_disagreeing_replace_the_second_count() {
        let mut bench = Bench::new(0.3e-6);
        for _ in 0..20 {
            bench.step();
        }
        let pulse = bench.pulse_ticks(bench.second - 1);
        for _ in 0..LABELS_REQUIRED {
            bench.servo.label(pulse, UNIX_START + bench.second + 99);
        }
        bench.servo.poll(pulse + TICK_RATE / 10);
        assert_eq!(
            bench.status.time_reference().seconds,
            UNIX_START + bench.second + 99 - REFERENCE_EPOCH_UNIX_SECONDS
        );
    }

    #[test]
    fn pulses_are_ignored_while_the_receiver_has_no_fix() {
        let mut bench = Bench::new(0.0);
        bench.servo.set_receiver(&report_with_fix(false));
        for _ in 0..20 {
            bench.step();
        }
        assert_eq!(bench.status.status().lock, ReferenceLock::Unlocked);
        assert_eq!(bench.status.status().accepted_pulses, 0);
    }

    #[test]
    fn missing_pulses_put_the_clock_in_holdover_and_grow_the_uncertainty() {
        let mut bench = Bench::new(1.0e-6);
        for _ in 0..40 {
            bench.step();
        }
        let locked = bench.status.time_reference().uncertainty_ns;
        bench.skip(4);
        let reference = bench.status.time_reference();
        assert_eq!(reference.lock, ReferenceLock::Holdover);
        bench.skip(60);
        let later = bench.status.time_reference();
        assert!(later.uncertainty_ns > reference.uncertainty_ns);
        assert!(reference.uncertainty_ns >= locked);
        // The learned rate keeps the clock close while the pulses are gone.
        let phase = bench.pulse_phase_ns(bench.second);
        assert!(phase.abs() < 2_000.0, "phase={phase} ns");
    }

    #[test]
    fn holdover_ends_when_pulses_return() {
        let mut bench = Bench::new(1.0e-6);
        for _ in 0..40 {
            bench.step();
        }
        bench.skip(4);
        assert_eq!(bench.status.status().lock, ReferenceLock::Holdover);
        bench.step();
        assert_eq!(bench.status.status().lock, ReferenceLock::Locked);
    }

    #[test]
    fn a_stray_pulse_is_rejected_without_moving_the_clock() {
        let mut bench = Bench::new(0.5e-6);
        for _ in 0..40 {
            bench.step();
        }
        let before = bench.status.status();
        let stray = bench.pulse_ticks(bench.second) + TICK_RATE / 1000;
        bench.servo.observe(stray, stray + TICK_RATE / 10);
        bench.servo.poll(stray + TICK_RATE / 10);
        let after = bench.status.status();
        assert_eq!(after.rejected_pulses, before.rejected_pulses + 1);
        assert_eq!(after.rate_error_ppb, before.rate_error_ppb);
        assert_eq!(after.lock, ReferenceLock::Locked);
    }

    #[test]
    fn a_reference_that_moves_restarts_tracking() {
        let mut bench = Bench::new(0.5e-6);
        for _ in 0..40 {
            bench.step();
        }
        bench.first_pulse += TICK_RATE as f64 / 100.0;
        for _ in 0..REJECT_LIMIT {
            bench.step();
        }
        assert_eq!(bench.status.status().lock, ReferenceLock::Unlocked);
        for _ in 0..20 {
            bench.step();
        }
        assert_eq!(bench.status.status().lock, ReferenceLock::Locked);
        let phase = bench.pulse_phase_ns(bench.second);
        assert!(phase.abs() < 200.0, "phase={phase} ns");
    }
}
