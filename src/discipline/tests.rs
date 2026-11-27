//! The disciplined halves against a device whose oscillator runs at a known error.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use num_complex::Complex32;
use parking_lot::Mutex;

use super::Shared;
use super::interp::Kernel;
use super::map::{Position, Segment, SharedMap};
use super::rx::DisciplinedRx;
use super::servo::SharedStatus;
use super::tx::DisciplinedTx;
use crate::error::Error;
use crate::{RadioRx, RadioTx, RxReadResult, Schedule, TxCapabilities, TxCapability, TxMetadata};

const TICK_RATE: u64 = 39_321_600;
const SAMPLE_RATE: f64 = 4_915_200.0;
const TICKS_PER_SAMPLE: f64 = TICK_RATE as f64 / SAMPLE_RATE;
const TONE_HZ: f64 = 300_000.0;
const CARRIER_HZ: f64 = 890_730_000.0;
/// The map starts here, after the device has run for a while.
const MAP_START: u64 = 100 * TICK_RATE;
const BLOCK: usize = 4096;

fn tone(seconds: f64) -> Complex32 {
    let phase = std::f64::consts::TAU * (TONE_HZ * seconds).fract();
    Complex32::new(phase.cos() as f32, phase.sin() as f32)
}

fn shared_with_rate(rate_error: f64) -> Arc<Shared> {
    let map = SharedMap::new();
    map.publish(Segment {
        device_start: MAP_START,
        disciplined_start: Position::from_ticks(MAP_START),
        rate_error,
    });
    Arc::new(Shared {
        map,
        status: SharedStatus::new(),
        stop: Arc::new(AtomicBool::new(false)),
        runner: std::sync::Mutex::new(None),
    })
}

/// Reference seconds since the map started, at a device tick.
fn reference_seconds(device_ticks: f64, rate_error: f64) -> f64 {
    (device_ticks - MAP_START as f64) / (1.0 + rate_error) / TICK_RATE as f64
}

#[derive(Default)]
struct Sent {
    /// Device tick of each sample written.
    samples: Vec<(f64, Complex32)>,
    /// First device tick and length of each write.
    writes: Vec<(u64, usize)>,
    ended: bool,
}

struct FakeTx {
    sent: Arc<Mutex<Sent>>,
    /// The rate the device applied, which need not be the requested one.
    rate_hz: f64,
    keep_samples: bool,
}

impl RadioTx for FakeTx {
    fn capabilities(&self) -> TxCapabilities {
        TxCapabilities::from([
            TxCapability::TimedSamples,
            TxCapability::BurstEnd,
            TxCapability::ClockReset,
        ])
    }
    fn write(&mut self, samples: &[Complex32], metadata: TxMetadata) -> Result<usize, Error> {
        let Schedule::At(time) = metadata.schedule else {
            return Err(Error::unsupported("untimed TX"));
        };
        let mut sent = self.sent.lock();
        sent.writes.push((time.ticks(), samples.len()));
        if self.keep_samples {
            let spacing = TICK_RATE as f64 / self.rate_hz;
            sent.samples.extend(
                samples
                    .iter()
                    .enumerate()
                    .map(|(index, sample)| (time.ticks() as f64 + index as f64 * spacing, *sample)),
            );
        }
        sent.ended |= metadata.end_of_burst;
        Ok(samples.len())
    }
    fn end_burst(&mut self) -> Result<(), Error> {
        self.sent.lock().ended = true;
        Ok(())
    }
    fn tx_sample_rate_hz(&self) -> f64 {
        self.rate_hz
    }
    fn supports_timed_tx(&self) -> bool {
        true
    }
    fn tick_rate(&self) -> u64 {
        TICK_RATE
    }
    fn get_hardware_time(&self) -> Result<u64, Error> {
        Ok(MAP_START + 3 * TICK_RATE)
    }
    fn transmit(&mut self, _samples: &[Complex32]) -> Result<(), Error> {
        Err(Error::unsupported("untimed TX"))
    }
    fn enable_transmit(&mut self, _enable: bool) -> Result<(), Error> {
        Ok(())
    }
}

fn disciplined_tx(rate_error: f64, carrier_hz: Option<f64>) -> (DisciplinedTx, Arc<Mutex<Sent>>) {
    let sent = Arc::new(Mutex::new(Sent::default()));
    let tx = DisciplinedTx::new(
        Box::new(FakeTx {
            sent: Arc::clone(&sent),
            rate_hz: SAMPLE_RATE,
            keep_samples: true,
        }),
        shared_with_rate(rate_error),
        Arc::new(Kernel::new()),
        carrier_hz,
    );
    (tx, sent)
}

/// Writes `blocks` blocks of the tone, each stamped with its disciplined time.
fn write_tone(tx: &mut DisciplinedTx, blocks: usize) {
    let start = MAP_START + TICK_RATE;
    for block in 0..blocks {
        let first = block * BLOCK;
        let samples: Vec<_> = (first..first + BLOCK)
            .map(|index| {
                tone((TICK_RATE as f64 + index as f64 * TICKS_PER_SAMPLE) / TICK_RATE as f64)
            })
            .collect();
        let ticks = start + (first as f64 * TICKS_PER_SAMPLE).round() as u64;
        tx.transmit_at(&samples, Some(ticks)).unwrap();
    }
}

#[test]
fn timestamped_halves_pass_samples_and_ticks_through_unchanged() {
    let sent = Arc::new(Mutex::new(Sent::default()));
    let mut tx = super::timestamps::TimestampedTx {
        inner: Box::new(FakeTx {
            sent: Arc::clone(&sent),
            rate_hz: SAMPLE_RATE,
            keep_samples: true,
        }),
        shared: shared_with_rate(0.0),
    };
    let samples: Vec<Complex32> = (0..BLOCK)
        .map(|index| Complex32::new(index as f32, -(index as f32)))
        .collect();
    let start = MAP_START + TICK_RATE;
    tx.write(
        &samples,
        TxMetadata {
            schedule: Schedule::At(crate::DeviceTime::from_ticks(start)),
            end_of_burst: false,
        },
    )
    .unwrap();
    let sent = sent.lock();
    assert_eq!(sent.writes, vec![(start, BLOCK)]);
    assert!(
        sent.samples
            .iter()
            .zip(&samples)
            .all(|((_, out), input)| out == input)
    );
    assert_eq!(tx.tx_sample_rate_hz(), SAMPLE_RATE);
    assert!(tx.time_reference().unwrap().is_some());
    assert!(tx.clock_status().is_some());
    assert!(tx.set_hardware_time(0).is_err());
    assert!(!tx.capabilities().supports(TxCapability::ClockReset));
}

#[test]
fn tx_samples_leave_the_device_at_the_reference_time_they_were_stamped_for() {
    for rate_error in [1.7e-6, -0.9e-6, 0.0] {
        let (mut tx, sent) = disciplined_tx(rate_error, None);
        write_tone(&mut tx, 40);
        let sent = sent.lock();
        assert!(sent.samples.len() > 39 * BLOCK);
        let worst = sent
            .samples
            .iter()
            .skip(64)
            .map(|(ticks, sample)| (*sample - tone(reference_seconds(*ticks, rate_error))).norm())
            .fold(0f32, f32::max);
        assert!(worst < 3e-3, "rate={rate_error} worst={worst}");
    }
}

#[test]
fn tx_output_is_one_gapless_stream_on_the_device_sample_grid() {
    let (mut tx, sent) = disciplined_tx(2.0e-6, None);
    write_tone(&mut tx, 40);
    let sent = sent.lock();
    for pair in sent.samples.windows(2) {
        let spacing = pair[1].0 - pair[0].0;
        assert!(
            (spacing - TICKS_PER_SAMPLE).abs() < 1e-6,
            "spacing={spacing}"
        );
    }
}

#[test]
fn a_device_rate_just_off_the_requested_one_keeps_one_stream() {
    // A device that applied a rate 90 parts per billion high. The client still writes
    // at the whole-hertz rate, half a sample away from the device grid within 1.2 s.
    const DEVICE_RATE_ERROR: f64 = 9e-8;
    const SECONDS: usize = 3;
    let sent = Arc::new(Mutex::new(Sent::default()));
    let mut tx = DisciplinedTx::new(
        Box::new(FakeTx {
            sent: Arc::clone(&sent),
            rate_hz: SAMPLE_RATE * (1.0 + DEVICE_RATE_ERROR),
            keep_samples: false,
        }),
        shared_with_rate(0.0),
        Arc::new(Kernel::new()),
        None,
    );
    assert_eq!(tx.tx_sample_rate_hz(), SAMPLE_RATE);
    let blocks = SECONDS * SAMPLE_RATE as usize / BLOCK;
    write_tone(&mut tx, blocks);
    tx.end_burst().unwrap();
    let sent = sent.lock();
    let device_ticks = TICKS_PER_SAMPLE / (1.0 + DEVICE_RATE_ERROR);
    for pair in sent.writes.windows(2) {
        let expected = pair[0].0 as f64 + pair[0].1 as f64 * device_ticks;
        let gap = pair[1].0 as f64 - expected;
        assert!(gap.abs() <= 1.0, "gap={gap} ticks");
    }
    let written: usize = sent.writes.iter().map(|write| write.1).sum();
    let given = (blocks * BLOCK) as f64;
    let extra = written as f64 - given;
    assert!(
        (extra - given * DEVICE_RATE_ERROR).abs() <= 20.0,
        "extra={extra}"
    );
}

#[test]
fn a_fast_device_sends_more_samples_than_it_was_given() {
    // Over one reference second a device that runs fast counts more of its own samples.
    let rate_error = 50e-6;
    let (mut tx, sent) = disciplined_tx(rate_error, None);
    let blocks = (SAMPLE_RATE as usize) / BLOCK;
    write_tone(&mut tx, blocks);
    tx.end_burst().unwrap();
    let sent = sent.lock();
    let extra = sent.samples.len() as f64 - (blocks * BLOCK) as f64;
    let expected = (blocks * BLOCK) as f64 * rate_error;
    assert!(
        (extra - expected).abs() <= 2.0,
        "extra={extra} expected={expected}"
    );
    assert!(sent.ended);
}

#[test]
fn tx_carrier_correction_cancels_the_oscillator_error() {
    let rate_error = 1.2e-6;
    let (mut tx, sent) = disciplined_tx(rate_error, Some(CARRIER_HZ));
    write_tone(&mut tx, 40);
    let sent = sent.lock();
    // On air the device adds the carrier it actually generates, which is high by the
    // oscillator error. The sum must be the tone on the requested carrier.
    let worst = sent
        .samples
        .iter()
        .skip(64)
        .map(|(ticks, sample)| {
            let seconds = reference_seconds(*ticks, rate_error);
            let excess = std::f64::consts::TAU * (CARRIER_HZ * rate_error * seconds).fract();
            let on_air = *sample * Complex32::new(excess.cos() as f32, excess.sin() as f32);
            (on_air - tone(seconds)).norm()
        })
        .fold(0f32, f32::max);
    assert!(worst < 5e-3, "worst={worst}");
}

#[test]
fn a_tx_write_that_does_not_follow_the_last_starts_a_new_stream() {
    let (mut tx, sent) = disciplined_tx(1.0e-6, None);
    write_tone(&mut tx, 2);
    let later = MAP_START + 5 * TICK_RATE;
    tx.transmit_at(&[Complex32::new(1.0, 0.0); BLOCK], Some(later))
        .unwrap();
    let sent = sent.lock();
    let jump = sent
        .samples
        .windows(2)
        .map(|pair| pair[1].0 - pair[0].0)
        .fold(0f64, f64::max);
    assert!(jump > TICK_RATE as f64, "jump={jump}");
}

#[test]
fn the_disciplined_clock_cannot_be_reset() {
    let (tx, _) = disciplined_tx(0.0, None);
    assert!(!tx.capabilities().supports(TxCapability::ClockReset));
    assert!(tx.capabilities().supports(TxCapability::TimedSamples));
    assert!(tx.set_hardware_time(0).is_err());
}

#[test]
fn hardware_time_is_reported_in_disciplined_ticks() {
    let rate_error = 10e-6;
    let (tx, _) = disciplined_tx(rate_error, None);
    let device = MAP_START + 3 * TICK_RATE;
    let expected = MAP_START as f64 + 3.0 * TICK_RATE as f64 / (1.0 + rate_error);
    let reported = tx.get_hardware_time().unwrap();
    assert!(
        (reported as f64 - expected).abs() <= 1.0,
        "device={device} reported={reported}"
    );
}

/// A device receiving the tone on a carrier, with an oscillator that runs fast.
struct FakeRx {
    rate_error: f64,
    carrier_hz: Option<f64>,
    next_sample: u64,
    overflow: Arc<AtomicBool>,
}

impl FakeRx {
    const FIRST_TICK: u64 = MAP_START + TICK_RATE + 3;
}

impl RadioRx for FakeRx {
    fn rx_sample_rate_hz(&self) -> f64 {
        SAMPLE_RATE
    }
    fn tick_rate(&self) -> u64 {
        TICK_RATE
    }
    fn get_hardware_time(&self) -> Result<u64, Error> {
        Ok(Self::FIRST_TICK)
    }
    fn rx_read(&mut self, buf: &mut [Complex32], _timeout_us: i64) -> Result<RxReadResult, Error> {
        let overflow = self
            .overflow
            .swap(false, std::sync::atomic::Ordering::AcqRel);
        if overflow {
            self.next_sample += 1000;
        }
        let first_ticks =
            Self::FIRST_TICK as f64 + (self.next_sample as f64 * TICKS_PER_SAMPLE).round();
        for (index, sample) in buf.iter_mut().enumerate() {
            let ticks = first_ticks + index as f64 * TICKS_PER_SAMPLE;
            let seconds = reference_seconds(ticks, self.rate_error);
            let low = self.carrier_hz.map_or(0.0, |carrier| {
                -std::f64::consts::TAU * (carrier * self.rate_error * seconds).fract()
            });
            *sample = tone(seconds) * Complex32::new(low.cos() as f32, low.sin() as f32);
        }
        self.next_sample += buf.len() as u64;
        Ok(RxReadResult {
            samples_read: buf.len(),
            time_ticks: first_ticks as u64,
            overflow,
        })
    }
    fn rx_activate(&mut self, _time_ticks: Option<u64>) -> Result<(), Error> {
        Ok(())
    }
    fn stop(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

fn disciplined_rx(rate_error: f64, carrier_hz: Option<f64>) -> DisciplinedRx {
    DisciplinedRx::new(
        Box::new(FakeRx {
            rate_error,
            carrier_hz,
            next_sample: 0,
            overflow: Arc::new(AtomicBool::new(false)),
        }),
        shared_with_rate(rate_error),
        Arc::new(Kernel::new()),
        carrier_hz,
    )
}

/// Reads `blocks` blocks and returns each sample with its disciplined tick.
fn read_blocks(rx: &mut DisciplinedRx, blocks: usize) -> Vec<(f64, Complex32)> {
    let mut samples = Vec::new();
    let mut buf = vec![Complex32::new(0.0, 0.0); BLOCK];
    for _ in 0..blocks {
        let read = rx.rx_read(&mut buf, 0).unwrap();
        samples.extend(
            buf[..read.samples_read]
                .iter()
                .enumerate()
                .map(|(index, sample)| {
                    (
                        read.time_ticks as f64 + index as f64 * TICKS_PER_SAMPLE,
                        *sample,
                    )
                }),
        );
    }
    samples
}

fn worst_rx_error(samples: &[(f64, Complex32)]) -> f32 {
    samples
        .iter()
        .skip(64)
        .map(|(ticks, sample)| {
            (*sample - tone((ticks - MAP_START as f64) / TICK_RATE as f64)).norm()
        })
        .fold(0f32, f32::max)
}

#[test]
fn rx_samples_are_stamped_with_the_reference_time_they_arrived() {
    for rate_error in [1.7e-6, -0.9e-6, 0.0] {
        let mut rx = disciplined_rx(rate_error, None);
        let samples = read_blocks(&mut rx, 40);
        assert!(samples.len() > 39 * BLOCK);
        let worst = worst_rx_error(&samples);
        assert!(worst < 3e-3, "rate={rate_error} worst={worst}");
    }
}

#[test]
fn rx_output_is_one_gapless_stream_on_whole_ticks() {
    let mut rx = disciplined_rx(2.0e-6, None);
    let samples = read_blocks(&mut rx, 40);
    for pair in samples.windows(2) {
        let spacing = pair[1].0 - pair[0].0;
        assert!(
            (spacing - TICKS_PER_SAMPLE).abs() < 1e-6,
            "spacing={spacing}"
        );
    }
}

#[test]
fn rx_carrier_correction_cancels_the_oscillator_error() {
    let mut rx = disciplined_rx(1.2e-6, Some(CARRIER_HZ));
    let samples = read_blocks(&mut rx, 40);
    let worst = worst_rx_error(&samples);
    assert!(worst < 5e-3, "worst={worst}");
}

#[test]
fn an_rx_overflow_is_reported_and_the_stream_resumes_on_time() {
    let rate_error = 1.0e-6;
    let overflow = Arc::new(AtomicBool::new(false));
    let mut rx = DisciplinedRx::new(
        Box::new(FakeRx {
            rate_error,
            carrier_hz: None,
            next_sample: 0,
            overflow: Arc::clone(&overflow),
        }),
        shared_with_rate(rate_error),
        Arc::new(Kernel::new()),
        None,
    );
    read_blocks(&mut rx, 4);
    overflow.store(true, std::sync::atomic::Ordering::Release);
    let mut buf = vec![Complex32::new(0.0, 0.0); BLOCK];
    let read = rx.rx_read(&mut buf, 0).unwrap();
    assert!(read.overflow);
    let after = read_blocks(&mut rx, 8);
    assert!(worst_rx_error(&after) < 3e-3);
}

/// Prints how fast the disciplined halves process samples. Run with `--ignored --nocapture`.
/// A receiver that hands out a constant block, so a benchmark times only the wrapper.
struct ConstantRx {
    next_sample: u64,
}

impl RadioRx for ConstantRx {
    fn rx_sample_rate_hz(&self) -> f64 {
        SAMPLE_RATE
    }
    fn tick_rate(&self) -> u64 {
        TICK_RATE
    }
    fn get_hardware_time(&self) -> Result<u64, Error> {
        Ok(FakeRx::FIRST_TICK)
    }
    fn rx_read(&mut self, buf: &mut [Complex32], _timeout_us: i64) -> Result<RxReadResult, Error> {
        buf.fill(Complex32::new(0.25, -0.25));
        let time_ticks = FakeRx::FIRST_TICK + self.next_sample * TICKS_PER_SAMPLE as u64;
        self.next_sample += buf.len() as u64;
        Ok(RxReadResult {
            samples_read: buf.len(),
            time_ticks,
            overflow: false,
        })
    }
    fn rx_activate(&mut self, _time_ticks: Option<u64>) -> Result<(), Error> {
        Ok(())
    }
    fn stop(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

/// Seconds of stream each benchmark case processes.
const BENCH_STREAM_SECONDS: usize = 4;
const BENCH_RATE_ERROR: f64 = -0.22e-6;

fn report_throughput(case: &str, samples: usize, seconds: f64) {
    let real_time = samples as f64 / SAMPLE_RATE / seconds;
    println!(
        "{case}: {:.1} Msps, {real_time:.1}x real time, {:.2}% of one core at {SAMPLE_RATE} sps",
        samples as f64 / seconds / 1e6,
        100.0 / real_time
    );
}

fn tx_throughput(carrier_hz: Option<f64>) -> (usize, f64) {
    let sent = Arc::new(Mutex::new(Sent::default()));
    let mut tx = DisciplinedTx::new(
        Box::new(FakeTx {
            sent: Arc::clone(&sent),
            rate_hz: SAMPLE_RATE,
            keep_samples: false,
        }),
        shared_with_rate(BENCH_RATE_ERROR),
        Arc::new(Kernel::new()),
        carrier_hz,
    );
    let block = vec![Complex32::new(0.25, -0.25); 24_576];
    let blocks = BENCH_STREAM_SECONDS * SAMPLE_RATE as usize / block.len();
    let start = MAP_START + TICK_RATE;
    let started = std::time::Instant::now();
    for index in 0..blocks {
        let ticks = start + (index * block.len()) as u64 * TICKS_PER_SAMPLE as u64;
        tx.transmit_at(&block, Some(ticks)).unwrap();
        sent.lock().writes.clear();
    }
    (blocks * block.len(), started.elapsed().as_secs_f64())
}

fn rx_throughput(carrier_hz: Option<f64>) -> (usize, f64) {
    let mut rx = DisciplinedRx::new(
        Box::new(ConstantRx { next_sample: 0 }),
        shared_with_rate(BENCH_RATE_ERROR),
        Arc::new(Kernel::new()),
        carrier_hz,
    );
    let mut buf = vec![Complex32::new(0.0, 0.0); 8192];
    let wanted = BENCH_STREAM_SECONDS * SAMPLE_RATE as usize;
    let mut samples = 0usize;
    let started = std::time::Instant::now();
    while samples < wanted {
        samples += rx.rx_read(&mut buf, 0).unwrap().samples_read;
    }
    (samples, started.elapsed().as_secs_f64())
}

#[test]
#[ignore = "timing printout, not an assertion"]
fn throughput() {
    println!(
        "interpolator kernel: {}",
        crate::dsp::fir::interleaved_dot_name()
    );
    for (case, carrier_hz) in [
        ("resample only", None),
        ("resample and rotate", Some(CARRIER_HZ)),
    ] {
        let (samples, seconds) = tx_throughput(carrier_hz);
        report_throughput(&format!("TX {case}"), samples, seconds);
        let (samples, seconds) = rx_throughput(carrier_hz);
        report_throughput(&format!("RX {case}"), samples, seconds);
    }
}
