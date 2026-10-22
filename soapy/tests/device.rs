//! The SoapySDR device lifecycle against a real `sdr-radiod` service and a fake radio.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use num_complex::Complex32;
use parking_lot::Mutex;
use sdr_hal::daemon::{CompositeConfig, RxDefaults, serve};
use sdr_hal::error::Error;
use sdr_hal::network::NetworkRadioOptions;
use sdr_hal::network::wire::Transport;
use sdr_hal::{Radio, RadioRx, RadioTx, RxReadResult};
use sdr_hal_soapy::{Device, Direction, ReadOutcome, TxEvent, TxTiming, WriteOutcome};

/// A clock that is not one tick per nanosecond, so a unit slip shows.
const TICK_RATE: u64 = 38_880_000;
const RATE_HZ: f64 = 972_000.0;
const RAMP_PERIOD: u64 = 1_000;
const TX_CENTER_HZ: u64 = 864_945_000;
const RX_CENTER_HZ: u64 = 819_945_000;
const DEVICE_RATE_HZ: u64 = 9_720_000;
const TIMEOUT: Duration = Duration::from_secs(2);
/// TX gain the fake radio supports, in dB.
const TX_GAIN_RANGE_DB: (f64, f64) = (0.0, 89.75);
/// TX gain the fake radio starts at, in dB.
const INITIAL_TX_GAIN_DB: f64 = 10.0;

fn apply_fake_tx_gain(state: &Mutex<FakeState>, gain_db: f64) -> f64 {
    let applied = gain_db.clamp(TX_GAIN_RANGE_DB.0, TX_GAIN_RANGE_DB.1);
    state.lock().tx_gain_db = Some(applied);
    applied
}

#[derive(Default)]
struct FakeState {
    tx_rate_hz: Option<f64>,
    tx_frequency_hz: Option<usize>,
    tx_bandwidth_hz: Option<usize>,
    rx_setup: Option<(String, f64, f64, Option<f64>)>,
    rx_frequency_hz: Option<f64>,
    rx_gain_db: Option<f64>,
    tx_gain_db: Option<f64>,
    enable_calls: Vec<bool>,
    transmits: Vec<(Option<u64>, usize)>,
    skip_next_rx: Option<u64>,
}

struct Clock(Instant);

impl Clock {
    fn now_ticks(&self) -> u64 {
        (self.0.elapsed().as_nanos() * TICK_RATE as u128 / 1_000_000_000) as u64
    }
}

struct FakeRadio {
    state: Arc<Mutex<FakeState>>,
    clock: Arc<Clock>,
}

impl Radio for FakeRadio {
    fn hardware_time(&self) -> Result<u64, Error> {
        Ok(self.clock.now_ticks())
    }
    fn tx_gain_range_db(&self) -> Option<(f64, f64)> {
        Some(TX_GAIN_RANGE_DB)
    }
    fn tx_gain_db(&self) -> Option<f64> {
        Some(self.state.lock().tx_gain_db.unwrap_or(INITIAL_TX_GAIN_DB))
    }
    fn set_tx_gain(&mut self, gain_db: f64) -> Result<f64, Error> {
        Ok(apply_fake_tx_gain(&self.state, gain_db))
    }
    fn tx_capabilities(&self) -> sdr_hal::TxCapabilities {
        sdr_hal::TxCapabilities::from([
            sdr_hal::TxCapability::TimedSamples,
            sdr_hal::TxCapability::TimedActivation,
            sdr_hal::TxCapability::BurstEnd,
        ])
    }
    fn tx_sample_rate_hz(&self) -> f64 {
        self.state.lock().tx_rate_hz.unwrap_or(0.0)
    }
    fn rx_sample_rate_hz(&self) -> Option<f64> {
        self.state.lock().rx_setup.as_ref().map(|setup| setup.1)
    }
    fn tick_rate(&self) -> u64 {
        TICK_RATE
    }
    fn set_tx_frequency(&mut self, frequency_hz: usize) -> Result<(), Error> {
        self.state.lock().tx_frequency_hz = Some(frequency_hz);
        Ok(())
    }
    fn set_tx_sample_rate(&mut self, rate_hz: f64) -> Result<f64, Error> {
        self.state.lock().tx_rate_hz = Some(rate_hz);
        Ok(rate_hz)
    }
    fn set_tx_bandwidth(&mut self, bandwidth_hz: usize) -> Result<(), Error> {
        self.state.lock().tx_bandwidth_hz = Some(bandwidth_hz);
        Ok(())
    }
    fn setup_rx(
        &mut self,
        _channel: usize,
        antenna: &str,
        rate_hz: f64,
        bandwidth_hz: f64,
        gain_db: Option<f64>,
    ) -> Result<f64, Error> {
        self.state.lock().rx_setup = Some((antenna.to_string(), rate_hz, bandwidth_hz, gain_db));
        Ok(rate_hz)
    }
    fn split(self: Box<Self>) -> Result<(Box<dyn RadioTx>, Option<Box<dyn RadioRx>>), Error> {
        let rx_rate_hz = self.rx_sample_rate_hz();
        Ok((
            Box::new(FakeTx {
                state: self.state.clone(),
                clock: self.clock.clone(),
            }),
            rx_rate_hz.map(|rate_hz| {
                Box::new(FakeRx {
                    state: self.state.clone(),
                    clock: self.clock.clone(),
                    rate_hz,
                    active: Arc::new(AtomicBool::new(false)),
                    next_sample: 0,
                }) as Box<dyn RadioRx>
            }),
        ))
    }
}

struct FakeTx {
    state: Arc<Mutex<FakeState>>,
    clock: Arc<Clock>,
}

impl RadioTx for FakeTx {
    fn capabilities(&self) -> sdr_hal::TxCapabilities {
        sdr_hal::TxCapabilities::from([
            sdr_hal::TxCapability::TimedSamples,
            sdr_hal::TxCapability::TimedActivation,
            sdr_hal::TxCapability::BurstEnd,
        ])
    }
    fn tx_sample_rate_hz(&self) -> f64 {
        self.state.lock().tx_rate_hz.unwrap_or(0.0)
    }
    fn supports_timed_tx(&self) -> bool {
        true
    }
    fn tick_rate(&self) -> u64 {
        TICK_RATE
    }
    fn get_hardware_time(&self) -> Result<u64, Error> {
        Ok(self.clock.now_ticks())
    }
    fn set_tx_frequency_hz(&mut self, frequency_hz: f64) -> Result<(), Error> {
        self.state.lock().tx_frequency_hz = Some(frequency_hz as usize);
        Ok(())
    }
    fn set_tx_gain(&mut self, gain_db: f64) -> Result<f64, Error> {
        Ok(apply_fake_tx_gain(&self.state, gain_db))
    }
    fn transmit(&mut self, samples: &[Complex32]) -> Result<(), Error> {
        self.state.lock().transmits.push((None, samples.len()));
        Ok(())
    }
    fn transmit_at(&mut self, samples: &[Complex32], tick: Option<u64>) -> Result<(), Error> {
        self.state.lock().transmits.push((tick, samples.len()));
        Ok(())
    }
    fn end_burst(&mut self) -> Result<(), Error> {
        Ok(())
    }
    fn enable_transmit(&mut self, enable: bool) -> Result<(), Error> {
        self.state.lock().enable_calls.push(enable);
        Ok(())
    }
}

struct FakeRx {
    state: Arc<Mutex<FakeState>>,
    clock: Arc<Clock>,
    rate_hz: f64,
    active: Arc<AtomicBool>,
    next_sample: u64,
}

impl FakeRx {
    fn ticks_at(&self, sample: u64) -> u64 {
        (sample as f64 * TICK_RATE as f64 / self.rate_hz) as u64
    }
}

impl RadioRx for FakeRx {
    fn rx_sample_rate_hz(&self) -> f64 {
        self.rate_hz
    }
    fn tick_rate(&self) -> u64 {
        TICK_RATE
    }
    fn get_hardware_time(&self) -> Result<u64, Error> {
        Ok(self.clock.now_ticks())
    }
    fn set_rx_frequency(&mut self, frequency_hz: f64) -> Result<(), Error> {
        self.state.lock().rx_frequency_hz = Some(frequency_hz);
        Ok(())
    }
    fn set_rx_gain(&mut self, gain_db: f64) -> Result<(), Error> {
        self.state.lock().rx_gain_db = Some(gain_db);
        Ok(())
    }
    fn rx_activate(&mut self, _time_ticks: Option<u64>) -> Result<(), Error> {
        self.next_sample = (self.clock.now_ticks() as f64 * self.rate_hz / TICK_RATE as f64) as u64;
        self.active.store(true, Ordering::Relaxed);
        Ok(())
    }
    fn stop(&mut self) -> Result<(), Error> {
        self.active.store(false, Ordering::Relaxed);
        Ok(())
    }
    /// A ramp of `n % RAMP_PERIOD`, paced to device time, so a reader can check
    /// continuity and timestamps from the samples alone.
    fn rx_read(&mut self, buf: &mut [Complex32], _timeout_us: i64) -> Result<RxReadResult, Error> {
        if !self.active.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(1));
            return Ok(RxReadResult {
                samples_read: 0,
                time_ticks: self.clock.now_ticks(),
                overflow: false,
            });
        }
        let skipped = self.state.lock().skip_next_rx.take();
        if let Some(skip) = skipped {
            self.next_sample += skip;
        }
        let start = self.next_sample;
        for (index, slot) in buf.iter_mut().enumerate() {
            *slot = ramp(start + index as u64);
        }
        self.next_sample += buf.len() as u64;
        let wait = self
            .ticks_at(self.next_sample)
            .saturating_sub(self.clock.now_ticks());
        std::thread::sleep(Duration::from_secs_f64(wait as f64 / TICK_RATE as f64));
        Ok(RxReadResult {
            samples_read: buf.len(),
            time_ticks: self.ticks_at(start),
            overflow: skipped.is_some(),
        })
    }
}

fn ramp(sample: u64) -> Complex32 {
    Complex32::new((sample % RAMP_PERIOD) as f32 / RAMP_PERIOD as f32, 0.0)
}

fn ramp_index(sample: Complex32) -> u64 {
    (sample.re * RAMP_PERIOD as f32).round() as u64 % RAMP_PERIOD
}

fn start_server(state: Arc<Mutex<FakeState>>, composite: Option<CompositeConfig>) -> SocketAddr {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let radio = Box::new(FakeRadio {
            state,
            clock: Arc::new(Clock(Instant::now())),
        });
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("server runtime");
        rt.block_on(async move {
            let (tonic_ready_tx, tonic_ready_rx) = tokio::sync::oneshot::channel();
            let localhost = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0);
            let serve_fut = serve(
                radio,
                true,
                "soapy-fake".to_string(),
                RxDefaults::default(),
                composite,
                localhost,
                localhost,
                Some(tonic_ready_tx),
                std::future::pending(),
            );
            tokio::pin!(serve_fut);
            tokio::select! {
                result = &mut serve_fut => panic!("server exited early: {result:?}"),
                ready = tonic_ready_rx => {
                    let (control, _data) = ready.expect("ready");
                    ready_tx.send(control).expect("send ready");
                }
            }
            let _ = serve_fut.await;
        });
    });
    ready_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("server ready")
}

fn open(control: SocketAddr) -> Device {
    let options = NetworkRadioOptions {
        transport: Transport::Tcp,
        ..NetworkRadioOptions::default()
    };
    Device::open(&control.to_string(), options).expect("device opens")
}

fn composite_config() -> CompositeConfig {
    CompositeConfig {
        device_rate_hz: DEVICE_RATE_HZ,
        tx_center_hz: TX_CENTER_HZ,
        rx_center_hz: RX_CENTER_HZ,
        tx_digital_backoff_db: 0.0,
        block_duration_ms: 2.0,
        tx_deadline_ms: 5.0,
    }
}

/// Reads until `count` samples arrive, skipping overflow reports.
fn read_samples(device: &Device, count: usize) -> Vec<(Complex32, i64)> {
    let mut out = Vec::new();
    let mut buf = vec![Complex32::default(); 4_096];
    let rate_ns = 1e9 / RATE_HZ;
    let deadline = Instant::now() + Duration::from_secs(10);
    while out.len() < count && Instant::now() < deadline {
        if let ReadOutcome::Samples { count, time_ns } = device.read(&mut buf, TIMEOUT).unwrap() {
            for (index, sample) in buf[..count].iter().enumerate() {
                out.push((*sample, time_ns + (index as f64 * rate_ns) as i64));
            }
        }
    }
    out
}

#[test]
fn device_time_reads_before_any_stream_exists() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let device = open(start_server(state, None));
    let first = device.hardware_time_ns().unwrap();
    std::thread::sleep(Duration::from_millis(20));
    let second = device.hardware_time_ns().unwrap();
    assert!(
        second > first,
        "device time did not advance: {first} then {second}"
    );
}

#[test]
fn polling_tx_events_before_tx_is_active_waits_out_the_timeout() {
    const POLL: Duration = Duration::from_millis(50);
    let state = Arc::new(Mutex::new(FakeState::default()));
    let device = open(start_server(state, None));
    let started = Instant::now();
    assert!(matches!(device.tx_event(POLL).unwrap(), TxEvent::Timeout));
    assert!(started.elapsed() >= POLL, "an inactive poll must not spin");
}

#[test]
fn a_fractional_tx_rate_passes_through_the_driver() {
    const FRACTIONAL_RATE_HZ: f64 = 3_250_000.0 / 3.0;
    let state = Arc::new(Mutex::new(FakeState::default()));
    let device = open(start_server(state.clone(), None));
    assert_eq!(
        device
            .set_sample_rate(Direction::Tx, FRACTIONAL_RATE_HZ)
            .unwrap(),
        FRACTIONAL_RATE_HZ
    );
    assert_eq!(state.lock().tx_rate_hz, Some(FRACTIONAL_RATE_HZ));
}

#[test]
fn settings_reach_the_server_before_the_session_and_rates_lock_after_it() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let device = open(start_server(state.clone(), None));
    assert_eq!(
        device.set_sample_rate(Direction::Tx, RATE_HZ).unwrap(),
        RATE_HZ
    );
    device.set_frequency(Direction::Tx, 869_040_000.0).unwrap();
    device.set_bandwidth(Direction::Tx, 30_000.0).unwrap();
    device.set_rx_antenna("RX2").unwrap();
    device.set_rx_gain(20.0).unwrap();
    assert_eq!(
        device.set_sample_rate(Direction::Rx, RATE_HZ).unwrap(),
        RATE_HZ
    );
    device.set_frequency(Direction::Rx, 824_040_000.0).unwrap();
    {
        let state = state.lock();
        assert_eq!(state.tx_rate_hz, Some(RATE_HZ));
        assert_eq!(state.tx_frequency_hz, Some(869_040_000));
        assert_eq!(state.tx_bandwidth_hz, Some(30_000));
        assert_eq!(
            state.rx_setup,
            Some(("RX2".to_string(), RATE_HZ, RATE_HZ, Some(20.0)))
        );
        assert_eq!(
            state.rx_frequency_hz, None,
            "RX tunes only once its stream exists"
        );
    }

    device.setup_stream(Direction::Tx).unwrap();
    device.setup_stream(Direction::Rx).unwrap();
    device.activate(Direction::Rx, None).unwrap();
    assert_eq!(state.lock().rx_frequency_hz, Some(824_040_000.0));

    assert!(
        device
            .set_sample_rate(Direction::Tx, 2.0 * RATE_HZ)
            .is_err()
    );
    assert_eq!(
        device.set_sample_rate(Direction::Tx, RATE_HZ).unwrap(),
        RATE_HZ
    );
    assert!(device.set_bandwidth(Direction::Rx, 10_000.0).is_err());
    device.set_frequency(Direction::Rx, 824_070_000.0).unwrap();
    device.set_rx_gain(30.0).unwrap();
    let state = state.lock();
    assert_eq!(state.rx_frequency_hz, Some(824_070_000.0));
    assert_eq!(state.rx_gain_db, Some(30.0));
}

#[test]
fn received_samples_are_contiguous_and_timed_by_the_device_clock() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let device = open(start_server(state, None));
    device.set_sample_rate(Direction::Rx, RATE_HZ).unwrap();
    device.setup_stream(Direction::Rx).unwrap();
    device.activate(Direction::Rx, None).unwrap();

    let wanted = (RATE_HZ / 2.0) as usize;
    let started = Instant::now();
    let samples = read_samples(&device, wanted);
    let elapsed = started.elapsed();
    assert!(
        samples.len() >= wanted,
        "read only {} samples",
        samples.len()
    );
    assert!(
        elapsed < Duration::from_millis(700),
        "half a second of samples took {elapsed:?}"
    );
    let tick_ns = 1e9 / TICK_RATE as f64;
    for pair in samples.windows(2) {
        let (first, second) = (ramp_index(pair[0].0), ramp_index(pair[1].0));
        assert_eq!(second, (first + 1) % RAMP_PERIOD, "a sample went missing");
        let step_ns = (pair[1].1 - pair[0].1) as f64;
        assert!(
            (step_ns - 1e9 / RATE_HZ).abs() <= tick_ns + 1.0,
            "timestamps stepped {step_ns} ns"
        );
    }
}

#[test]
fn a_gap_is_reported_as_overflow_before_the_samples_after_it() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let device = open(start_server(state.clone(), None));
    device.set_sample_rate(Direction::Rx, RATE_HZ).unwrap();
    device.setup_stream(Direction::Rx).unwrap();
    device.activate(Direction::Rx, None).unwrap();
    let before = read_samples(&device, 2_000);
    let last_before = ramp_index(before.last().unwrap().0);

    state.lock().skip_next_rx = Some(250);
    let mut buf = vec![Complex32::default(); 512];
    let mut seen_overflow = false;
    let mut expected_next = (last_before + 1) % RAMP_PERIOD;
    for _ in 0..200 {
        match device.read(&mut buf, TIMEOUT).unwrap() {
            ReadOutcome::Overflow => {
                seen_overflow = true;
            }
            ReadOutcome::Samples { count, .. } => {
                let first = ramp_index(buf[0]);
                if seen_overflow {
                    assert_ne!(first, expected_next, "the gap's samples were not skipped");
                    return;
                }
                assert_eq!(
                    first, expected_next,
                    "samples were lost without an overflow"
                );
                expected_next = (ramp_index(buf[count - 1]) + 1) % RAMP_PERIOD;
            }
            ReadOutcome::Timeout => panic!("the stream stalled"),
        }
    }
    panic!("no overflow was reported for the gap");
}

#[test]
fn tx_gain_reaches_a_single_client_server_before_and_during_streaming() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let device = open(start_server(state.clone(), None));
    assert_eq!(device.tx_gain_range(), Some(TX_GAIN_RANGE_DB));
    assert_eq!(device.tx_gain(), Some(INITIAL_TX_GAIN_DB));
    assert_eq!(device.set_tx_gain(30.0).unwrap(), 30.0);
    assert_eq!(state.lock().tx_gain_db, Some(30.0));

    device.set_sample_rate(Direction::Tx, RATE_HZ).unwrap();
    device.setup_stream(Direction::Tx).unwrap();
    device.activate(Direction::Tx, None).unwrap();
    assert_eq!(device.set_tx_gain(120.0).unwrap(), TX_GAIN_RANGE_DB.1);
    assert_eq!(state.lock().tx_gain_db, Some(TX_GAIN_RANGE_DB.1));
    assert_eq!(device.tx_gain(), Some(TX_GAIN_RANGE_DB.1));
}

#[test]
fn a_composite_server_keeps_tx_gain_to_itself() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let device = open(start_server(state.clone(), Some(composite_config())));
    assert_eq!(device.tx_gain_range(), None);
    assert_eq!(device.tx_gain(), None);
    assert!(device.set_tx_gain(30.0).is_err());
    assert_eq!(state.lock().tx_gain_db, None);
}

#[test]
fn untimed_writes_wait_for_device_time_instead_of_running_ahead() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let device = open(start_server(state, None));
    device.set_sample_rate(Direction::Tx, RATE_HZ).unwrap();
    device.setup_stream(Direction::Tx).unwrap();
    device.activate(Direction::Tx, None).unwrap();

    let chunk = vec![Complex32::new(0.5, 0.0); 9_720];
    let started = Instant::now();
    for _ in 0..50 {
        let outcome = device
            .write(&chunk, TxTiming::Untimed, false, TIMEOUT)
            .unwrap();
        assert_eq!(outcome, WriteOutcome::Written(chunk.len()));
    }
    let elapsed = started.elapsed();
    // Half a second of samples, less the lead untimed writes may run ahead by.
    assert!(
        elapsed >= Duration::from_millis(350) && elapsed < Duration::from_millis(700),
        "half a second of untimed samples took {elapsed:?}"
    );
    assert_eq!(
        device
            .write(&chunk, TxTiming::Untimed, false, Duration::ZERO)
            .unwrap(),
        WriteOutcome::Timeout,
        "a write that would run too far ahead returns at once with a zero timeout"
    );
}

#[test]
fn a_timed_write_reaches_the_device_at_its_time() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let device = open(start_server(state.clone(), None));
    device.set_sample_rate(Direction::Tx, RATE_HZ).unwrap();
    device.setup_stream(Direction::Tx).unwrap();
    device.activate(Direction::Tx, None).unwrap();
    let when_ns = device.hardware_time_ns().unwrap() + 50_000_000;
    let chunk = vec![Complex32::new(0.5, 0.0); 1_000];
    device
        .write(&chunk, TxTiming::At(when_ns), true, TIMEOUT)
        .unwrap();
    let expected_ticks = (when_ns as u128 * TICK_RATE as u128).div_ceil(1_000_000_000) as u64;
    let deadline = Instant::now() + TIMEOUT;
    while state.lock().transmits.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(20));
    let transmits = state.lock().transmits.clone();
    let tick = transmits[0].0.expect("the block went out timed");
    assert!(
        tick.abs_diff(expected_ticks) <= 1,
        "{tick} vs {expected_ticks}"
    );
    // The burst-end marker is sent with the final sample, so the block can arrive in two parts.
    let total: usize = transmits.iter().map(|(_, len)| len).sum();
    assert_eq!(total, chunk.len());
}

#[test]
fn deactivating_tx_stops_the_device_before_the_silence_watchdog() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let device = open(start_server(state.clone(), None));
    device.set_sample_rate(Direction::Tx, RATE_HZ).unwrap();
    device.setup_stream(Direction::Tx).unwrap();
    device.activate(Direction::Tx, None).unwrap();
    let chunk = vec![Complex32::new(0.5, 0.0); 9_720];
    for _ in 0..5 {
        device
            .write(&chunk, TxTiming::Untimed, false, TIMEOUT)
            .unwrap();
    }
    device.deactivate(Direction::Tx).unwrap();
    device.close_stream(Direction::Tx).unwrap();
    let stopped = Instant::now();
    while !state.lock().enable_calls.contains(&false)
        && stopped.elapsed() < Duration::from_millis(400)
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        state.lock().enable_calls.contains(&false),
        "TX was still on {:?} after deactivation",
        stopped.elapsed()
    );
}

#[test]
fn a_receive_only_device_streams_without_a_tx_rate() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let device = open(start_server(state.clone(), None));
    device.set_sample_rate(Direction::Rx, RATE_HZ).unwrap();
    device.setup_stream(Direction::Rx).unwrap();
    device.activate(Direction::Rx, None).unwrap();
    assert!(read_samples(&device, 10_000).len() >= 10_000);
    assert!(device.setup_stream(Direction::Tx).is_err());
    assert_eq!(state.lock().tx_rate_hz, None);
}

#[test]
fn closing_every_stream_lets_a_composite_client_stream_again_at_a_new_rate() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let device = open(start_server(state, Some(composite_config())));
    let ranges = device.ranges().clone();
    assert_eq!(
        ranges.tx_sample_rate_hz,
        Some(DEVICE_RATE_HZ.div_ceil(4096) as f64..=DEVICE_RATE_HZ as f64)
    );
    let half_span = (DEVICE_RATE_HZ / 2) as f64;
    assert_eq!(
        ranges.rx_frequency_hz,
        Some(RX_CENTER_HZ as f64 - half_span..=RX_CENTER_HZ as f64 + half_span)
    );

    device.set_sample_rate(Direction::Rx, RATE_HZ).unwrap();
    device
        .set_frequency(Direction::Rx, RX_CENTER_HZ as f64)
        .unwrap();
    device.setup_stream(Direction::Rx).unwrap();
    device.activate(Direction::Rx, None).unwrap();
    assert!(read_samples(&device, 5_000).len() >= 5_000);
    device.deactivate(Direction::Rx).unwrap();
    device.close_stream(Direction::Rx).unwrap();

    let new_rate = RATE_HZ / 2.0;
    assert_eq!(
        device.set_sample_rate(Direction::Rx, new_rate).unwrap(),
        new_rate
    );
    device.setup_stream(Direction::Rx).unwrap();
    device.activate(Direction::Rx, None).unwrap();
    let mut buf = vec![Complex32::default(); 1_024];
    let deadline = Instant::now() + TIMEOUT;
    let mut received = 0;
    while received < 5_000 && Instant::now() < deadline {
        if let ReadOutcome::Samples { count, .. } = device.read(&mut buf, TIMEOUT).unwrap() {
            received += count;
        }
    }
    assert!(received >= 5_000, "the second session received {received}");
    assert_eq!(device.sample_rate(Direction::Rx), new_rate);
}
