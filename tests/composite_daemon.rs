//! Two unmodified clients sharing one device stream through composite mode.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use num_complex::Complex32;
use parking_lot::Mutex;
use sdr_hal::daemon::{CompositeConfig, RxDefaults, serve};
use sdr_hal::error::Error;
use sdr_hal::network::wire::Transport;
use sdr_hal::network::{NetworkRadio, NetworkRadioOptions};
use sdr_hal::{Radio, RadioRx, RadioTx, RxReadResult};

/// Two carriers 3.04 MHz apart about a shared center.
const DEVICE_RATE_HZ: u64 = 3_888_000;
const TICK_RATE: u64 = 31_104_000;
const TX_LEAD_TICKS: u64 = TICK_RATE / 10;
const TX_CENTER_HZ: u64 = 867_520_000;
const RX_CENTER_HZ: u64 = 822_520_000;
const CLIENT_RATE_HZ: u64 = 388_800;
const RATIO: usize = 10;
const GRID_BLOCK: usize = 3_240;
const LOW_TX_HZ: f64 = 866_000_000.0;
const HIGH_TX_HZ: f64 = 869_040_000.0;
const LOW_RX_HZ: f64 = 821_000_000.0;
const HIGH_RX_HZ: f64 = 824_040_000.0;
const OFFSET_HZ: i64 = 1_520_000;

#[derive(Default)]
struct DeviceState {
    tx_frequency: Option<usize>,
    tx_sample_rate: Option<usize>,
    tx_bandwidth: Option<usize>,
    rx_setup: Option<(usize, String, f64, f64, Option<f64>)>,
    rx_frequency_hz: Option<f64>,
    enable_calls: Vec<bool>,
    transmits: Vec<(Option<u64>, Vec<Complex32>)>,
    rx_activated: bool,
    rx_stops: usize,
}

struct Clock {
    start: Instant,
}

impl Clock {
    fn now_ticks(&self) -> u64 {
        (self.start.elapsed().as_nanos() * TICK_RATE as u128 / 1_000_000_000) as u64
    }
}

struct Device {
    state: Arc<Mutex<DeviceState>>,
    clock: Arc<Clock>,
    active: Arc<AtomicBool>,
}

impl Radio for Device {
    fn tx_capabilities(&self) -> sdr_hal::TxCapabilities {
        sdr_hal::TxCapabilities::from([
            sdr_hal::TxCapability::TimedSamples,
            sdr_hal::TxCapability::TimedActivation,
            sdr_hal::TxCapability::ClockReset,
        ])
    }
    fn set_tx_frequency(&mut self, frequency_hz: usize) -> Result<(), Error> {
        self.state.lock().tx_frequency = Some(frequency_hz);
        Ok(())
    }
    fn set_tx_sample_rate(&mut self, sample_rate_hz: f64) -> Result<f64, Error> {
        self.state.lock().tx_sample_rate = Some(sample_rate_hz as usize);
        Ok(sample_rate_hz)
    }
    fn set_tx_bandwidth(&mut self, bandwidth_hz: usize) -> Result<(), Error> {
        self.state.lock().tx_bandwidth = Some(bandwidth_hz);
        Ok(())
    }
    fn tx_sample_rate_hz(&self) -> f64 {
        self.state.lock().tx_sample_rate.unwrap_or(0) as f64
    }
    fn rx_sample_rate_hz(&self) -> Option<f64> {
        self.state.lock().rx_setup.as_ref().map(|setup| setup.2)
    }
    fn tick_rate(&self) -> u64 {
        TICK_RATE
    }
    fn setup_rx(
        &mut self,
        channel: usize,
        antenna: &str,
        sample_rate_hz: f64,
        bandwidth_hz: f64,
        gain_db: Option<f64>,
    ) -> Result<f64, Error> {
        self.state.lock().rx_setup = Some((
            channel,
            antenna.to_string(),
            sample_rate_hz,
            bandwidth_hz,
            gain_db,
        ));
        Ok(sample_rate_hz)
    }
    fn split(self: Box<Self>) -> Result<(Box<dyn RadioTx>, Option<Box<dyn RadioRx>>), Error> {
        Ok((
            Box::new(DeviceTx {
                state: self.state.clone(),
                clock: self.clock.clone(),
            }),
            Some(Box::new(DeviceRx {
                state: self.state.clone(),
                clock: self.clock.clone(),
                active: self.active.clone(),
                next_sample: 0,
            })),
        ))
    }
}

struct DeviceTx {
    state: Arc<Mutex<DeviceState>>,
    clock: Arc<Clock>,
}

struct DeviceClock(Arc<Clock>);

impl sdr_hal::RadioClock for DeviceClock {
    fn get_hardware_time(&self) -> Result<u64, Error> {
        Ok(self.0.now_ticks())
    }
}

impl RadioTx for DeviceTx {
    fn clock_reader(&self) -> Option<Box<dyn sdr_hal::RadioClock>> {
        Some(Box::new(DeviceClock(self.clock.clone())))
    }

    fn capabilities(&self) -> sdr_hal::TxCapabilities {
        sdr_hal::TxCapabilities::from([
            sdr_hal::TxCapability::TimedSamples,
            sdr_hal::TxCapability::TimedActivation,
            sdr_hal::TxCapability::ClockReset,
        ])
    }
    fn tx_sample_rate_hz(&self) -> f64 {
        DEVICE_RATE_HZ as f64
    }
    fn set_tx_frequency_hz(&mut self, frequency_hz: f64) -> Result<(), Error> {
        self.state.lock().tx_frequency = Some(frequency_hz as usize);
        Ok(())
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
    fn transmit(&mut self, samples: &[Complex32]) -> Result<(), Error> {
        self.state.lock().transmits.push((None, samples.to_vec()));
        Ok(())
    }
    fn transmit_at(&mut self, samples: &[Complex32], tick: Option<u64>) -> Result<(), Error> {
        self.state.lock().transmits.push((tick, samples.to_vec()));
        Ok(())
    }
    fn enable_transmit(&mut self, enable: bool) -> Result<(), Error> {
        self.state.lock().enable_calls.push(enable);
        Ok(())
    }
}

struct DeviceRx {
    state: Arc<Mutex<DeviceState>>,
    clock: Arc<Clock>,
    active: Arc<AtomicBool>,
    next_sample: u64,
}

impl RadioRx for DeviceRx {
    fn rx_sample_rate_hz(&self) -> f64 {
        DEVICE_RATE_HZ as f64
    }
    fn tick_rate(&self) -> u64 {
        TICK_RATE
    }
    fn set_rx_frequency(&mut self, frequency_hz: f64) -> Result<(), Error> {
        self.state.lock().rx_frequency_hz = Some(frequency_hz);
        Ok(())
    }
    fn set_rx_gain(&mut self, _gain_db: f64) -> Result<(), Error> {
        Ok(())
    }
    fn get_hardware_time(&self) -> Result<u64, Error> {
        Ok(self.clock.now_ticks())
    }
    fn rx_activate(&mut self, _time_ticks: Option<u64>) -> Result<(), Error> {
        self.next_sample = self.clock.now_ticks() / (TICK_RATE / DEVICE_RATE_HZ);
        self.state.lock().rx_activated = true;
        self.active.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn stop(&mut self) -> Result<(), Error> {
        self.state.lock().rx_stops += 1;
        self.active.store(false, Ordering::SeqCst);
        Ok(())
    }
    /// Both uplink carriers, the low one at 0.8 and the high one at 0.3.
    fn rx_read(&mut self, buf: &mut [Complex32], _timeout_us: i64) -> Result<RxReadResult, Error> {
        if !self.active.load(Ordering::SeqCst) {
            return Ok(RxReadResult {
                samples_read: 0,
                time_ticks: self.clock.now_ticks(),
                overflow: false,
            });
        }
        let start = self.next_sample;
        for (index, slot) in buf.iter_mut().enumerate() {
            let n = (start + index as u64) as f64;
            let low = std::f64::consts::TAU * -(OFFSET_HZ as f64) * n / DEVICE_RATE_HZ as f64;
            let high = std::f64::consts::TAU * OFFSET_HZ as f64 * n / DEVICE_RATE_HZ as f64;
            *slot = Complex32::new(0.8 * low.cos() as f32, 0.8 * low.sin() as f32)
                + Complex32::new(0.3 * high.cos() as f32, 0.3 * high.sin() as f32);
        }
        self.next_sample += buf.len() as u64;
        let end_ticks = self.next_sample * (TICK_RATE / DEVICE_RATE_HZ);
        let remaining_ticks = end_ticks.saturating_sub(self.clock.now_ticks());
        std::thread::sleep(Duration::from_secs_f64(
            remaining_ticks as f64 / TICK_RATE as f64,
        ));
        Ok(RxReadResult {
            samples_read: buf.len(),
            time_ticks: start * (TICK_RATE / DEVICE_RATE_HZ),
            overflow: false,
        })
    }
}

fn composite_config() -> CompositeConfig {
    CompositeConfig {
        device_rate_hz: DEVICE_RATE_HZ,
        tx_center_hz: TX_CENTER_HZ,
        rx_center_hz: RX_CENTER_HZ,
        tx_digital_backoff_db: 0.0,
        block_duration_ms: 5.0 / 6.0,
        tx_deadline_ms: 2.0,
    }
}

fn start_server(state: Arc<Mutex<DeviceState>>) -> (std::thread::JoinHandle<()>, SocketAddr) {
    start_server_with_config(state, composite_config())
}

fn start_server_with_config(
    state: Arc<Mutex<DeviceState>>,
    config: CompositeConfig,
) -> (std::thread::JoinHandle<()>, SocketAddr) {
    let _ = env_logger::builder().is_test(false).try_init();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let radio = Box::new(Device {
            state,
            clock: Arc::new(Clock {
                start: Instant::now(),
            }),
            active: Arc::new(AtomicBool::new(false)),
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
                "composite-fake".to_string(),
                RxDefaults::default(),
                Some(config),
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
    let control = ready_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("server ready");
    (handle, control)
}

/// Opens one client the way an unmodified consumer does: rates, then split, then tune.
fn open_client(
    control: SocketAddr,
    tx_hz: f64,
    rx_hz: f64,
    with_rx: bool,
) -> (Box<dyn RadioTx>, Option<Box<dyn RadioRx>>) {
    open_client_over(control, tx_hz, rx_hz, with_rx, Transport::default())
}

fn open_client_over(
    control: SocketAddr,
    tx_hz: f64,
    rx_hz: f64,
    with_rx: bool,
    transport: Transport,
) -> (Box<dyn RadioTx>, Option<Box<dyn RadioRx>>) {
    let options = NetworkRadioOptions {
        transport,
        ..NetworkRadioOptions::default()
    };
    let mut radio =
        Box::new(NetworkRadio::connect(&control.to_string(), options).expect("client connects"));
    radio.set_tx_bandwidth(30_000).expect("tx bandwidth");
    let applied = radio
        .set_tx_sample_rate(CLIENT_RATE_HZ as f64)
        .expect("tx rate");
    assert_eq!(
        applied, CLIENT_RATE_HZ as f64,
        "a composite client keeps its own rate"
    );
    assert_eq!(
        radio.tick_rate(),
        TICK_RATE,
        "every client shares the device clock"
    );
    if with_rx {
        let applied = radio
            .setup_rx(0, "", CLIENT_RATE_HZ as f64, 30_000.0, None)
            .expect("setup rx");
        assert_eq!(applied, CLIENT_RATE_HZ as f64);
    }
    let (mut tx, rx) = (radio as Box<dyn Radio>).split().expect("split");
    tx.set_tx_frequency_hz(tx_hz).expect("tx tune");
    let rx = if with_rx {
        let mut rx = rx.expect("rx half");
        rx.set_rx_frequency(rx_hz).expect("rx tune");
        Some(rx)
    } else {
        None
    };
    (tx, rx)
}

/// Mean magnitude of `samples`' component at `offset_hz`, by coherent integration.
fn tone(samples: &[Complex32], offset_hz: i64, rate_hz: u64) -> f32 {
    let step = -std::f64::consts::TAU * offset_hz as f64 / rate_hz as f64;
    let mut sum = Complex32::default();
    for (n, &sample) in samples.iter().enumerate() {
        let phase = step * n as f64;
        sum += sample * Complex32::new(phase.cos() as f32, phase.sin() as f32);
    }
    sum.norm() / samples.len() as f32
}

#[test]
fn the_device_opens_configured_from_the_operator_plan() {
    let state = Arc::new(Mutex::new(DeviceState::default()));
    let (_server, control) = start_server(state.clone());
    let (_tx, _rx) = open_client(control, LOW_TX_HZ, LOW_RX_HZ, true);

    let state = state.lock();
    // The device runs at the composite rate and the shared centers, not the client's.
    assert_eq!(state.tx_sample_rate, Some(DEVICE_RATE_HZ as usize));
    assert_eq!(state.tx_frequency, Some(TX_CENTER_HZ as usize));
    assert_eq!(state.rx_frequency_hz, Some(RX_CENTER_HZ as f64));
    let rx = state.rx_setup.as_ref().expect("the daemon sets RX up");
    assert_eq!(rx.2, DEVICE_RATE_HZ as f64);
    // The client's 30 kHz bandwidth request never reaches the analog filter, which
    // matters because 30 kHz at the shared center would filter out its own carrier.
    assert_ne!(state.tx_bandwidth, Some(30_000));
}

#[test]
fn two_clients_land_at_their_own_carriers_in_one_summed_stream() {
    let state = Arc::new(Mutex::new(DeviceState::default()));
    let (_server, control) = start_server(state.clone());

    let (mut low, _) = open_client(control, LOW_TX_HZ, LOW_RX_HZ, false);
    let (mut high, _) = open_client(control, HIGH_TX_HZ, HIGH_RX_HZ, false);
    low.enable_transmit(true).expect("low transmit");
    high.enable_transmit(true).expect("high transmit");

    // Both clients send a steady tone at their own rate, scheduled well ahead.
    let per_block = GRID_BLOCK / RATIO;
    let samples = vec![Complex32::new(1.0, 0.0); per_block];
    let ticks_per_client_sample = TICK_RATE / CLIENT_RATE_HZ;
    let start = low.get_hardware_time().expect("device time") + TX_LEAD_TICKS;
    for block in 0..12u64 {
        let tick = start + block * per_block as u64 * ticks_per_client_sample;
        low.transmit_at(&samples, Some(tick)).expect("low tx");
        high.transmit_at(&samples, Some(tick)).expect("high tx");
        std::thread::sleep(Duration::from_millis(3));
    }
    std::thread::sleep(Duration::from_millis(120));

    let transmits = state.lock().transmits.clone();
    assert!(
        transmits.len() >= 4,
        "the grid emitted only {} block(s)",
        transmits.len()
    );
    for (tick, block) in &transmits {
        assert!(tick.is_some(), "a grid block went out untimed");
        assert_eq!(block.len(), GRID_BLOCK, "grid blocks are a fixed length");
    }
    // Grid blocks are contiguous in device time, which is what lets them be summed.
    let block_ticks = GRID_BLOCK as u64 * (TICK_RATE / DEVICE_RATE_HZ);
    for pair in transmits.windows(2) {
        let (Some(first), Some(second)) = (pair[0].0, pair[1].0) else {
            panic!("untimed grid block");
        };
        assert_eq!(
            second,
            first + block_ticks,
            "grid blocks are not contiguous"
        );
    }

    // Each client's tone sits at its own offset. The first and last blocks a client
    // spans are partly silent, because its stream does not begin on a block boundary,
    // so the fullest block is the one that shows the steady-state level.
    let levels: Vec<(f32, f32, f32)> = transmits
        .iter()
        .map(|(_, block)| {
            (
                tone(block, -OFFSET_HZ, DEVICE_RATE_HZ),
                tone(block, OFFSET_HZ, DEVICE_RATE_HZ),
                tone(block, 0, DEVICE_RATE_HZ),
            )
        })
        .collect();
    let best = levels
        .iter()
        .copied()
        .max_by(|a, b| a.0.min(a.1).total_cmp(&b.0.min(b.1)))
        .expect("at least one block");
    assert!(
        best.0 > 0.9 && best.1 > 0.9,
        "carriers reached only {best:?} across {levels:?}"
    );
    assert!(
        (best.0 - best.1).abs() < 0.05,
        "clients summed at unequal gain: {best:?}"
    );
    assert!(
        best.2 < 0.05,
        "energy leaked to the shared center: {best:?}"
    );
}

#[test]
fn each_client_receives_only_its_own_carrier() {
    let state = Arc::new(Mutex::new(DeviceState::default()));
    let (_server, control) = start_server(state.clone());

    let (_low_tx, low_rx) = open_client(control, LOW_TX_HZ, LOW_RX_HZ, true);
    let (_high_tx, high_rx) = open_client(control, HIGH_TX_HZ, HIGH_RX_HZ, true);
    let mut low_rx = low_rx.expect("low rx");
    let mut high_rx = high_rx.expect("high rx");
    low_rx.rx_activate(None).expect("low activate");
    high_rx.rx_activate(None).expect("high activate");
    // The device receiver starts once, however many clients want it.
    std::thread::sleep(Duration::from_millis(50));
    assert!(state.lock().rx_activated);

    let drain = |rx: &mut Box<dyn RadioRx>| -> Vec<Complex32> {
        let mut collected = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buf = vec![Complex32::default(); 4096];
        while collected.len() < 8_192 && Instant::now() < deadline {
            let read = rx.rx_read(&mut buf, 200_000).expect("rx read");
            collected.extend_from_slice(&buf[..read.samples_read]);
        }
        collected
    };

    let low_samples = drain(&mut low_rx);
    let high_samples = drain(&mut high_rx);
    assert!(
        low_samples.len() >= 8_192 && high_samples.len() >= 8_192,
        "only {} and {} samples arrived",
        low_samples.len(),
        high_samples.len()
    );

    // Each client's carrier arrives at baseband at its own amplitude, past the
    // transient at the start of its slice filter.
    let low_level = tone(&low_samples[2_048..], 0, CLIENT_RATE_HZ);
    let high_level = tone(&high_samples[2_048..], 0, CLIENT_RATE_HZ);
    assert!(
        (low_level - 0.8).abs() < 0.08,
        "low received {low_level} rather than 0.8"
    );
    assert!(
        (high_level - 0.3).abs() < 0.08,
        "high received {high_level} rather than 0.3"
    );
}

#[test]
fn one_client_stopping_leaves_its_peer_running() {
    let state = Arc::new(Mutex::new(DeviceState::default()));
    let (_server, control) = start_server(state.clone());

    let (mut low, low_rx) = open_client(control, LOW_TX_HZ, LOW_RX_HZ, true);
    let (mut high, high_rx) = open_client(control, HIGH_TX_HZ, HIGH_RX_HZ, true);
    let mut low_rx = low_rx.expect("low rx");
    let mut high_rx = high_rx.expect("high rx");

    low.enable_transmit(true).expect("low transmit");
    high.enable_transmit(true).expect("high transmit");
    low_rx.rx_activate(None).expect("low activate");
    high_rx.rx_activate(None).expect("high activate");
    std::thread::sleep(Duration::from_millis(30));

    // One client going away must not stop the device under its peer.
    low.enable_transmit(false).expect("low stop");
    low_rx.stop().expect("low rx stop");
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(
        state.lock().rx_stops,
        0,
        "the device receiver stopped while a peer still wanted it"
    );
    assert_eq!(
        state.lock().enable_calls.iter().filter(|&&on| !on).count(),
        0,
        "the device transmitter stopped while a peer still wanted it"
    );

    // The last client leaving does stop them.
    high.enable_transmit(false).expect("high stop");
    high_rx.stop().expect("high rx stop");
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(state.lock().rx_stops, 1);
    assert!(
        state.lock().enable_calls.contains(&false),
        "the device transmitter never stopped"
    );
}

/// Sends one grid block's worth of samples at `tick`, as a live client would.
fn send_block(tx: &mut Box<dyn RadioTx>, tick: u64) {
    let samples = vec![Complex32::new(1.0, 0.0); GRID_BLOCK / RATIO];
    tx.transmit_at(&samples, Some(tick)).expect("transmit");
}

#[test]
fn a_transmitter_that_dies_does_not_leave_the_device_radiating() {
    let state = Arc::new(Mutex::new(DeviceState::default()));
    let (_server, control) = start_server(state.clone());
    let (mut low, _) = open_client(control, LOW_TX_HZ, LOW_RX_HZ, false);
    low.enable_transmit(true).expect("transmit");

    let ticks_per_client_sample = TICK_RATE / CLIENT_RATE_HZ;
    let per_block = (GRID_BLOCK / RATIO) as u64;
    let start = low.get_hardware_time().expect("device time") + TX_LEAD_TICKS;
    for block in 0..4u64 {
        send_block(
            &mut low,
            start + block * per_block * ticks_per_client_sample,
        );
        std::thread::sleep(Duration::from_millis(3));
    }
    assert!(
        state.lock().enable_calls.contains(&true),
        "the device never started transmitting"
    );

    // The client stops sending without saying so, which is what a crash looks like.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !state.lock().enable_calls.contains(&false) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        state.lock().enable_calls.contains(&false),
        "a dead client left the device transmitting: {:?}",
        state.lock().enable_calls
    );
}

#[test]
fn a_silent_client_does_not_stop_the_device_under_a_peer_still_sending() {
    let state = Arc::new(Mutex::new(DeviceState::default()));
    let (_server, control) = start_server(state.clone());
    let (mut low, _) = open_client(control, LOW_TX_HZ, LOW_RX_HZ, false);
    let (mut high, _) = open_client(control, HIGH_TX_HZ, HIGH_RX_HZ, false);
    low.enable_transmit(true).expect("low transmit");
    high.enable_transmit(true).expect("high transmit");

    let ticks_per_client_sample = TICK_RATE / CLIENT_RATE_HZ;
    let per_block = (GRID_BLOCK / RATIO) as u64;
    let start = high.get_hardware_time().expect("device time") + TX_LEAD_TICKS;
    // The low client sends only at the start, while the high client keeps going well past the
    // silence timeout, so the watchdog fires for one slice and not the other.
    send_block(&mut low, start);
    for block in 0..90u64 {
        send_block(
            &mut high,
            start + block * per_block * ticks_per_client_sample,
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(
        !state.lock().enable_calls.contains(&false),
        "a peer still sending had the device stopped under it: {:?}",
        state.lock().enable_calls
    );
    // A slice released while a peer transmits lets a new client take that slot.
    let (mut rejoined, _) = open_client(control, LOW_TX_HZ, LOW_RX_HZ, false);
    rejoined.enable_transmit(true).expect("rejoined transmit");
    let tick = rejoined.get_hardware_time().expect("device time") + TX_LEAD_TICKS;
    send_block(&mut rejoined, tick);
}

#[test]
fn a_receive_only_client_that_drops_its_stream_releases_its_slice() {
    let state = Arc::new(Mutex::new(DeviceState::default()));
    let (_server, control) = start_server(state.clone());

    // A TCP data stream stays open for the client's whole life, so dropping the client
    // is a definitive report that it left even though it never transmits.
    let (keeper_tx, keeper_rx) =
        open_client_over(control, LOW_TX_HZ, LOW_RX_HZ, true, Transport::Tcp);
    let mut keeper_rx = keeper_rx.expect("keeper rx");
    keeper_rx.rx_activate(None).expect("keeper activate");

    {
        let (_leaver_tx, leaver_rx) =
            open_client_over(control, HIGH_TX_HZ, HIGH_RX_HZ, true, Transport::Tcp);
        let mut leaver_rx = leaver_rx.expect("leaver rx");
        leaver_rx.rx_activate(None).expect("leaver activate");
        let mut buf = vec![Complex32::default(); 2_048];
        let _ = leaver_rx.rx_read(&mut buf, 200_000);
        // Dropped here without calling rx_stop, which is what a crash looks like.
    }

    // The surviving client keeps receiving its own carrier throughout.
    let mut collected = Vec::new();
    let mut buf = vec![Complex32::default(); 4_096];
    let deadline = Instant::now() + Duration::from_secs(5);
    while collected.len() < 8_192 && Instant::now() < deadline {
        let read = keeper_rx.rx_read(&mut buf, 200_000).expect("keeper read");
        collected.extend_from_slice(&buf[..read.samples_read]);
    }
    assert!(
        collected.len() >= 8_192,
        "keeper stalled after its peer left"
    );
    let level = tone(&collected[2_048..], 0, CLIENT_RATE_HZ);
    assert!(
        (level - 0.8).abs() < 0.08,
        "keeper reads {level} rather than 0.8 after its peer left"
    );

    // A new client reuses the released slice rather than adding one, which it can only
    // do if the slot came back.
    let (_rejoined_tx, rejoined_rx) =
        open_client_over(control, HIGH_TX_HZ, HIGH_RX_HZ, true, Transport::Tcp);
    let mut rejoined_rx = rejoined_rx.expect("rejoined rx");
    rejoined_rx.rx_activate(None).expect("rejoined activate");
    let mut rejoined = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while rejoined.len() < 8_192 && Instant::now() < deadline {
        let read = rejoined_rx
            .rx_read(&mut buf, 200_000)
            .expect("rejoined read");
        rejoined.extend_from_slice(&buf[..read.samples_read]);
    }
    assert!(rejoined.len() >= 8_192, "the rejoining client got nothing");
    let level = tone(&rejoined[2_048..], 0, CLIENT_RATE_HZ);
    assert!(
        (level - 0.3).abs() < 0.08,
        "rejoining client reads {level} rather than 0.3"
    );
    drop(keeper_tx);
}

#[test]
fn a_receive_only_udp_client_that_dies_releases_its_slice() {
    let state = Arc::new(Mutex::new(DeviceState::default()));
    let (_server, control) = start_server(state.clone());

    // UDP reports nothing when a client that only receives goes away, so the control
    // connection closing is the only signal. It identifies the client anyway.
    let (keeper_tx, keeper_rx) =
        open_client_over(control, LOW_TX_HZ, LOW_RX_HZ, true, Transport::Udp);
    let mut keeper_rx = keeper_rx.expect("keeper rx");
    keeper_rx.rx_activate(None).expect("keeper activate");

    {
        let (_leaver_tx, leaver_rx) =
            open_client_over(control, HIGH_TX_HZ, HIGH_RX_HZ, true, Transport::Udp);
        let mut leaver_rx = leaver_rx.expect("leaver rx");
        leaver_rx.rx_activate(None).expect("leaver activate");
        let mut buf = vec![Complex32::default(); 2_048];
        let _ = leaver_rx.rx_read(&mut buf, 200_000);
        // Dropped here without stopping anything, taking its control channel with it.
    }

    let drain = |rx: &mut Box<dyn RadioRx>| -> Vec<Complex32> {
        let mut collected = Vec::new();
        let mut buf = vec![Complex32::default(); 4_096];
        let deadline = Instant::now() + Duration::from_secs(5);
        while collected.len() < 8_192 && Instant::now() < deadline {
            let read = rx.rx_read(&mut buf, 200_000).expect("rx read");
            collected.extend_from_slice(&buf[..read.samples_read]);
        }
        collected
    };

    // The surviving client is unaffected.
    let kept = drain(&mut keeper_rx);
    assert!(kept.len() >= 8_192, "keeper stalled after its peer left");
    let level = tone(&kept[2_048..], 0, CLIENT_RATE_HZ);
    assert!((level - 0.8).abs() < 0.08, "keeper reads {level}");

    // A new client reuses the released slice, which it can only do if the slot came
    // back when the control connection closed.
    let (_rejoined_tx, rejoined_rx) =
        open_client_over(control, HIGH_TX_HZ, HIGH_RX_HZ, true, Transport::Udp);
    let mut rejoined_rx = rejoined_rx.expect("rejoined rx");
    rejoined_rx.rx_activate(None).expect("rejoined activate");
    let rejoined = drain(&mut rejoined_rx);
    assert!(rejoined.len() >= 8_192, "the rejoining client got nothing");
    let level = tone(&rejoined[2_048..], 0, CLIENT_RATE_HZ);
    assert!(
        (level - 0.3).abs() < 0.08,
        "rejoining client reads {level} rather than 0.3"
    );
    drop(keeper_tx);
}

#[test]
fn two_tcp_clients_both_reach_their_own_carrier() {
    let state = Arc::new(Mutex::new(DeviceState::default()));
    let (_server, control) = start_server(state.clone());

    // One reader per client, so the first client's stream cannot block the second's
    // data plane from connecting at all.
    let (_low_tx, low_rx) = open_client_over(control, LOW_TX_HZ, LOW_RX_HZ, true, Transport::Tcp);
    let (_high_tx, high_rx) =
        open_client_over(control, HIGH_TX_HZ, HIGH_RX_HZ, true, Transport::Tcp);
    let mut low_rx = low_rx.expect("low rx");
    let mut high_rx = high_rx.expect("high rx");
    low_rx.rx_activate(None).expect("low activate");
    high_rx.rx_activate(None).expect("high activate");

    let drain = |rx: &mut Box<dyn RadioRx>| -> Vec<Complex32> {
        let mut collected = Vec::new();
        let mut buf = vec![Complex32::default(); 4_096];
        let deadline = Instant::now() + Duration::from_secs(5);
        while collected.len() < 8_192 && Instant::now() < deadline {
            let read = rx.rx_read(&mut buf, 200_000).expect("rx read");
            collected.extend_from_slice(&buf[..read.samples_read]);
        }
        collected
    };
    let low_samples = drain(&mut low_rx);
    let high_samples = drain(&mut high_rx);
    assert!(
        low_samples.len() >= 8_192,
        "the first TCP client got only {} samples",
        low_samples.len()
    );
    assert!(
        high_samples.len() >= 8_192,
        "the second TCP client got only {} samples",
        high_samples.len()
    );

    let low_level = tone(&low_samples[2_048..], 0, CLIENT_RATE_HZ);
    let high_level = tone(&high_samples[2_048..], 0, CLIENT_RATE_HZ);
    assert!((low_level - 0.8).abs() < 0.08, "low reads {low_level}");
    assert!((high_level - 0.3).abs() < 0.08, "high reads {high_level}");
}

#[test]
fn stopping_all_clients_stops_grid_writes_and_a_new_burst_uses_its_own_time() {
    let state = Arc::new(Mutex::new(DeviceState::default()));
    let (_server, control) = start_server(state.clone());
    let (mut client, _) = open_client_over(control, LOW_TX_HZ, LOW_RX_HZ, false, Transport::Tcp);
    for _ in 0..3 {
        client.enable_transmit(false).unwrap();
    }
    client.enable_transmit(true).unwrap();
    let start = client.get_hardware_time().unwrap() + TX_LEAD_TICKS;
    send_block(&mut client, start);
    std::thread::sleep(Duration::from_millis(100));
    client.enable_transmit(false).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    let stopped_count = state.lock().transmits.len();
    std::thread::sleep(Duration::from_millis(40));
    assert!(
        stopped_count > 0,
        "startup stop fences must precede the first burst"
    );
    assert_eq!(state.lock().transmits.len(), stopped_count);

    client.enable_transmit(true).unwrap();
    let restart = client.get_hardware_time().unwrap() + TX_LEAD_TICKS;
    send_block(&mut client, restart);
    std::thread::sleep(Duration::from_millis(100));
    client.enable_transmit(false).unwrap();
    let transmits = state.lock().transmits.clone();
    assert!(transmits.len() > stopped_count);
    let first_restart = transmits[stopped_count].0.unwrap();
    let block_ticks = GRID_BLOCK as u64 * (TICK_RATE / DEVICE_RATE_HZ);
    assert_eq!(first_restart, restart / block_ticks * block_ticks);
}

#[test]
fn a_rational_client_rate_is_accepted_without_rounding() {
    let state = Arc::new(Mutex::new(DeviceState::default()));
    let (_server, control) = start_server(state);
    let mut radio = Box::new(
        NetworkRadio::connect(&control.to_string(), NetworkRadioOptions::default()).unwrap(),
    );
    assert_eq!(radio.set_tx_sample_rate(100_000.0).unwrap(), 100_000.0);
}

#[test]
fn a_fractional_rx_bandwidth_is_accepted() {
    const FRACTIONAL_RATE_HZ: f64 = DEVICE_RATE_HZ as f64 / 7.0;
    let state = Arc::new(Mutex::new(DeviceState::default()));
    let (_server, control) = start_server(state);
    let mut radio = Box::new(
        NetworkRadio::connect(&control.to_string(), NetworkRadioOptions::default()).unwrap(),
    );
    let applied = radio
        .setup_rx(0, "", FRACTIONAL_RATE_HZ, FRACTIONAL_RATE_HZ, None)
        .expect("setup rx");
    assert!((applied - FRACTIONAL_RATE_HZ).abs() < 1e-6, "{applied}");
}

#[test]
fn a_client_clock_request_is_ignored_and_answered_with_the_device_clock() {
    let state = Arc::new(Mutex::new(DeviceState::default()));
    let (_server, control) = start_server(state);
    let mut radio = Box::new(
        NetworkRadio::connect(&control.to_string(), NetworkRadioOptions::default()).unwrap(),
    );
    assert_eq!(radio.set_tick_rate(TICK_RATE * 2).unwrap(), TICK_RATE);
    assert_eq!(radio.tick_rate(), TICK_RATE);
}

#[test]
fn the_shared_clock_epoch_cannot_be_reset_by_a_client() {
    let state = Arc::new(Mutex::new(DeviceState::default()));
    let (_server, control) = start_server(state);
    let (tx, _) = open_client(control, LOW_TX_HZ, LOW_RX_HZ, false);
    // The device advertises a resettable clock, but composite mode withholds that
    // capability, so a well-behaved client refuses before it even asks.
    let error = tx
        .set_hardware_time(0)
        .expect_err("the shared epoch is not a client's to move")
        .to_string();
    assert!(error.contains("not supported"), "{error}");
}

#[test]
fn a_carrier_outside_the_shared_span_is_refused() {
    let state = Arc::new(Mutex::new(DeviceState::default()));
    let (_server, control) = start_server(state);
    let mut radio = Box::new(
        NetworkRadio::connect(&control.to_string(), NetworkRadioOptions::default())
            .expect("client connects"),
    );
    radio
        .set_tx_sample_rate(CLIENT_RATE_HZ as f64)
        .expect("tx rate");
    let (mut tx, _) = (radio as Box<dyn Radio>).split().expect("split");
    let error = tx
        .set_tx_frequency_hz(880_000_000.0)
        .expect_err("a carrier past the span edge is refused")
        .to_string();
    assert!(error.contains("half-span"), "{error}");
    tx.set_tx_frequency_hz(LOW_TX_HZ).expect("a carrier inside");
}

#[test]
fn rational_client_joins_integer_clients_over_tcp_with_tx_and_rx() {
    const RATIONAL_RATE: usize = 1_228_800;
    const RATIONAL_BLOCK: usize = RATIONAL_RATE / 200;
    let state = Arc::new(Mutex::new(DeviceState::default()));
    let (_server, control) = start_server(state.clone());
    let (_low, _) = open_client(control, LOW_TX_HZ, LOW_RX_HZ, false);
    let (_high, _) = open_client(control, HIGH_TX_HZ, HIGH_RX_HZ, false);
    let mut radio = Box::new(
        NetworkRadio::connect(
            &control.to_string(),
            NetworkRadioOptions {
                transport: Transport::Tcp,
                ..NetworkRadioOptions::default()
            },
        )
        .unwrap(),
    );
    assert_eq!(
        radio.set_tx_sample_rate(RATIONAL_RATE as f64).unwrap(),
        RATIONAL_RATE as f64
    );
    radio.set_tx_bandwidth(500_000).unwrap();
    radio
        .setup_rx(0, "", RATIONAL_RATE as f64, 500_000.0, None)
        .unwrap();
    let (mut tx, rx) = (radio as Box<dyn Radio>).split().unwrap();
    tx.set_tx_frequency_hz(TX_CENTER_HZ as f64).unwrap();
    let mut rx = rx.unwrap();
    rx.set_rx_frequency(RX_CENTER_HZ as f64).unwrap();
    rx.rx_activate(None).unwrap();
    tx.enable_transmit(true).unwrap();
    let start = tx.get_hardware_time().unwrap() + TX_LEAD_TICKS;
    let input = vec![Complex32::new(0.5, 0.0); RATIONAL_BLOCK];
    for block in 0..12u64 {
        tx.transmit_at(&input, Some(start + block * TICK_RATE / 200))
            .unwrap();
    }
    let mut output = vec![Complex32::default(); RATIONAL_BLOCK];
    let first = rx.rx_read(&mut output, 1_000_000).unwrap();
    let second = rx.rx_read(&mut output, 1_000_000).unwrap();
    assert_eq!(first.samples_read, RATIONAL_BLOCK);
    assert_eq!(second.samples_read, RATIONAL_BLOCK);
    assert!(!first.overflow && !second.overflow);
    assert!(
        second
            .time_ticks
            .abs_diff(first.time_ticks + TICK_RATE / 200)
            <= 1
    );
    std::thread::sleep(Duration::from_millis(180));
    tx.enable_transmit(false).unwrap();
    rx.stop().unwrap();
    let transmits = state.lock().transmits.clone();
    let peak = transmits
        .iter()
        .map(|(_, samples)| tone(samples, 0, DEVICE_RATE_HZ))
        .fold(0.0f32, f32::max);
    assert!((peak - 0.5).abs() < 0.01, "rational TX amplitude={peak}");
}

#[test]
fn configured_block_duration_controls_device_writes() {
    let state = Arc::new(Mutex::new(DeviceState::default()));
    let config = CompositeConfig {
        block_duration_ms: 2.0,
        tx_deadline_ms: 5.0,
        ..composite_config()
    };
    let (_server, control) = start_server_with_config(state.clone(), config);
    let (mut tx, _) = open_client_over(control, LOW_TX_HZ, LOW_RX_HZ, false, Transport::Tcp);
    tx.enable_transmit(true).unwrap();
    let start = tx.get_hardware_time().unwrap() + TX_LEAD_TICKS;
    let upstream = vec![Complex32::new(0.2, 0.0); CLIENT_RATE_HZ as usize / 25];
    tx.transmit_at(&upstream, Some(start)).unwrap();
    std::thread::sleep(Duration::from_millis(180));
    tx.enable_transmit(false).unwrap();
    let state = state.lock();
    assert!(!state.transmits.is_empty());
    assert!(
        state
            .transmits
            .iter()
            .all(|(_, samples)| samples.len() == 7776)
    );
}

#[test]
fn a_block_that_splits_client_samples_still_serves_every_client() {
    // 1 ms is 3,888 device samples, which neither 1,228,800 Hz (405/128) nor 388,800 Hz
    // (10/1) divides into, and the rational client connects first.
    const RATIONAL_RATE: usize = 1_228_800;
    const BLOCK: usize = 3_888;
    let state = Arc::new(Mutex::new(DeviceState::default()));
    let config = CompositeConfig {
        block_duration_ms: 1.0,
        tx_deadline_ms: 2.0,
        ..composite_config()
    };
    let (_server, control) = start_server_with_config(state.clone(), config);
    let mut radio = Box::new(
        NetworkRadio::connect(&control.to_string(), NetworkRadioOptions::default()).unwrap(),
    );
    radio.set_tx_sample_rate(RATIONAL_RATE as f64).unwrap();
    radio
        .setup_rx(0, "", RATIONAL_RATE as f64, 500_000.0, None)
        .unwrap();
    let (_rational_tx, rational_rx) = (radio as Box<dyn Radio>).split().unwrap();
    let mut rational_rx = rational_rx.unwrap();
    rational_rx.set_rx_frequency(RX_CENTER_HZ as f64).unwrap();
    rational_rx.rx_activate(None).unwrap();
    let (mut low_tx, low_rx) = open_client(control, LOW_TX_HZ, LOW_RX_HZ, true);
    let mut low_rx = low_rx.unwrap();
    low_rx.rx_activate(None).unwrap();

    low_tx.enable_transmit(true).unwrap();
    let start = low_tx.get_hardware_time().unwrap() + TX_LEAD_TICKS;
    let input = vec![Complex32::new(0.5, 0.0); CLIENT_RATE_HZ as usize / 50];
    low_tx.transmit_at(&input, Some(start)).unwrap();
    for rx in [&mut rational_rx, &mut low_rx] {
        let mut output = vec![Complex32::default(); 4_096];
        let first = rx.rx_read(&mut output, 1_000_000).unwrap();
        let second = rx.rx_read(&mut output, 1_000_000).unwrap();
        let period = (first.samples_read as u128 * TICK_RATE as u128
            / rx.rx_sample_rate_hz() as u128) as u64;
        // A reader starved by host load sees a gap, which it is told about.
        assert!(
            second.overflow || second.time_ticks.abs_diff(first.time_ticks + period) <= 1,
            "rate {} first {:?} {} second {:?} {} period {period}",
            rx.rx_sample_rate_hz(),
            first.time_ticks,
            first.samples_read,
            second.time_ticks,
            second.samples_read
        );
    }
    std::thread::sleep(Duration::from_millis(180));
    low_tx.enable_transmit(false).unwrap();
    rational_rx.stop().unwrap();
    low_rx.stop().unwrap();
    let transmits = state.lock().transmits.clone();
    assert!(transmits.iter().all(|(_, samples)| samples.len() == BLOCK));
    let peak = transmits
        .iter()
        .map(|(_, samples)| tone(samples, -OFFSET_HZ, DEVICE_RATE_HZ))
        .fold(0.0f32, f32::max);
    assert!((peak - 0.5).abs() < 0.01, "low TX amplitude={peak}");
}

#[test]
fn an_untimed_client_sums_beside_a_timed_one() {
    let state = Arc::new(Mutex::new(DeviceState::default()));
    let (_server, control) = start_server(state.clone());

    let (mut low, _) = open_client(control, LOW_TX_HZ, LOW_RX_HZ, false);
    let (mut high, _) = open_client(control, HIGH_TX_HZ, HIGH_RX_HZ, false);
    low.enable_transmit(true).expect("low transmit");
    high.enable_transmit(true).expect("high transmit");

    let per_block = GRID_BLOCK / RATIO;
    let untimed = std::thread::spawn(move || {
        let samples = vec![Complex32::new(1.0, 0.0); per_block];
        for _ in 0..240 {
            high.transmit(&samples).expect("untimed tx");
        }
        high
    });
    let samples = vec![Complex32::new(1.0, 0.0); per_block];
    let ticks_per_client_sample = TICK_RATE / CLIENT_RATE_HZ;
    let start = low.get_hardware_time().expect("device time") + TX_LEAD_TICKS;
    for block in 0..12u64 {
        let tick = start + block * per_block as u64 * ticks_per_client_sample;
        low.transmit_at(&samples, Some(tick)).expect("low tx");
        std::thread::sleep(Duration::from_millis(3));
    }
    let _high = untimed.join().expect("untimed sender");
    std::thread::sleep(Duration::from_millis(120));

    let transmits = state.lock().transmits.clone();
    let best = transmits
        .iter()
        .map(|(_, block)| {
            (
                tone(block, -OFFSET_HZ, DEVICE_RATE_HZ),
                tone(block, OFFSET_HZ, DEVICE_RATE_HZ),
            )
        })
        .max_by(|a, b| a.0.min(a.1).total_cmp(&b.0.min(b.1)))
        .expect("the grid emitted blocks");
    assert!(
        best.0 > 0.9 && best.1 > 0.9,
        "the timed and untimed carriers never shared a full block: {best:?}"
    );
}

#[test]
fn a_client_without_tx_cannot_switch_a_peer_transmitter() {
    let state = Arc::new(Mutex::new(DeviceState::default()));
    let (_server, control) = start_server(state.clone());
    let (mut low, _) = open_client(control, LOW_TX_HZ, LOW_RX_HZ, false);
    low.enable_transmit(true).expect("low transmit");

    let mut listener = Box::new(
        NetworkRadio::connect(&control.to_string(), NetworkRadioOptions::default())
            .expect("listener connects"),
    );
    listener
        .setup_rx(0, "", CLIENT_RATE_HZ as f64, 30_000.0, None)
        .expect("listener rx");
    let (mut listener_tx, listener_rx) = (listener as Box<dyn Radio>)
        .split()
        .expect("a receive-only composite client opens");
    let mut listener_rx = listener_rx.expect("listener rx half");
    listener_rx
        .set_rx_frequency(HIGH_RX_HZ)
        .expect("listener tune");

    let error = listener_tx
        .enable_transmit(true)
        .expect_err("a client with no TX stream cannot transmit");
    assert!(error.to_string().contains("no TX stream"), "{error}");
    listener_tx
        .enable_transmit(false)
        .expect("stopping a stream that does not exist is a no-op");
    std::thread::sleep(Duration::from_millis(30));
    assert!(
        !state.lock().enable_calls.contains(&false),
        "a receive-only client stopped its peer's transmitter: {:?}",
        state.lock().enable_calls
    );
    low.enable_transmit(false).expect("low stop");
}

#[test]
fn clients_learn_the_rates_and_frequencies_the_plan_accepts() {
    let state = Arc::new(Mutex::new(DeviceState::default()));
    let (_server, control) = start_server(state);
    let radio = NetworkRadio::connect(&control.to_string(), NetworkRadioOptions::default())
        .expect("client connects");
    let ranges = radio.accepted_ranges();
    let half_span = (DEVICE_RATE_HZ / 2) as f64;
    let rates = Some(DEVICE_RATE_HZ.div_ceil(4096) as f64..=DEVICE_RATE_HZ as f64);
    assert_eq!(ranges.tx_sample_rate_hz, rates);
    assert_eq!(ranges.rx_sample_rate_hz, rates);
    assert_eq!(
        ranges.tx_frequency_hz,
        Some(TX_CENTER_HZ as f64 - half_span..=TX_CENTER_HZ as f64 + half_span)
    );
    assert_eq!(
        ranges.rx_frequency_hz,
        Some(RX_CENTER_HZ as f64 - half_span..=RX_CENTER_HZ as f64 + half_span)
    );
}
