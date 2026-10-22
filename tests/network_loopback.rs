use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use num_complex::Complex32;
use parking_lot::Mutex;
use sdr_hal::daemon::{RadioCalibration, RxDefaults, serve};
use sdr_hal::error::Error;
use sdr_hal::network::wire::Transport;
use sdr_hal::network::{NetworkRadio, NetworkRadioOptions};
use sdr_hal::{CalibratedRadio, Radio, RadioRx, RadioTx, RxReadResult};

/// A device clock that is not one nanosecond per tick, so a unit slip would show.
const INNER_TICK_RATE: u64 = 49_152_000;
const RX_TICK_RATE: u64 = INNER_TICK_RATE;
const TX_SAMPLE_DELAY: i64 = -70;
const TX_RATE_HZ: u64 = 4_915_200;
const TX_BLOCK_SAMPLES: usize = 12_288;
const RX_RATE_HZ: f64 = 4_915_200.0;
const RX_CHUNK: usize = 2048;

const COMPOSITE_TX_RATE_HZ: u64 = 9_830_400;
const COMPOSITE_BANDWIDTH_HZ: usize = 5_890_000;
const COMPOSITE_RX_CENTER_HZ: f64 = 846_105_000.0;
const COMPOSITE_BLOCK_SAMPLES: usize = 24_576;

#[derive(Default)]
struct FakeState {
    applied_rx_rate_hz: Option<f64>,
    applied_tx_rate_hz: Option<f64>,
    coupled_rates: bool,
    burst_ends: usize,
    enable_calls: usize,
    timed_tx_supported: Option<bool>,
    tx_frequency: Option<usize>,
    tx_sample_rate: Option<f64>,
    tx_bandwidth: Option<usize>,
    rx_setup: Option<(usize, String, f64, f64, Option<f64>)>,
    rx_frequency_hz: Option<f64>,
    transmit_enabled: Option<(bool, Option<u64>)>,
    transmits: Vec<(Option<u64>, Vec<Complex32>)>,
    rx_activated: bool,
    stale_rx_blocks: usize,
    rx_tick_rate: Option<u64>,
    tick_rate: Option<u64>,
    tx_write_delay: Option<Duration>,
}

struct FakeClock {
    start: Instant,
}

impl FakeClock {
    fn now_ticks(&self) -> u64 {
        (self.start.elapsed().as_nanos() * INNER_TICK_RATE as u128 / 1_000_000_000) as u64
    }
}

struct FakeRadio {
    state: Arc<Mutex<FakeState>>,
    clock: Arc<FakeClock>,
}

impl Radio for FakeRadio {
    fn tx_capabilities(&self) -> sdr_hal::TxCapabilities {
        let capabilities = sdr_hal::TxCapabilities::from([sdr_hal::TxCapability::BurstEnd]);
        if self.supports_timed_tx() {
            capabilities
                .with(sdr_hal::TxCapability::TimedSamples)
                .with(sdr_hal::TxCapability::TimedActivation)
        } else {
            capabilities
        }
    }

    fn tx_sample_rate_hz(&self) -> f64 {
        {
            let state = self.state.lock();
            state
                .applied_tx_rate_hz
                .unwrap_or(state.tx_sample_rate.unwrap_or_default())
        }
    }
    fn rx_sample_rate_hz(&self) -> Option<f64> {
        let state = self.state.lock();
        state
            .rx_setup
            .as_ref()
            .map(|setup| state.applied_rx_rate_hz.unwrap_or(setup.2))
    }
    fn supports_timed_tx(&self) -> bool {
        self.state.lock().timed_tx_supported.unwrap_or(true)
    }
    fn tick_rate(&self) -> u64 {
        let state = self.state.lock();
        if state.tx_sample_rate.is_none() {
            1
        } else {
            state.tick_rate.unwrap_or(INNER_TICK_RATE)
        }
    }
    fn set_tick_rate(&mut self, tick_rate: u64) -> Result<u64, Error> {
        // Model a driver that only lands on whole megahertz.
        let applied = (tick_rate / 1_000_000).max(1) * 1_000_000;
        self.state.lock().tick_rate = Some(applied);
        Ok(applied)
    }

    fn set_tx_frequency(&mut self, center_frequency: usize) -> Result<(), Error> {
        self.state.lock().tx_frequency = Some(center_frequency);
        Ok(())
    }
    fn set_tx_sample_rate(&mut self, sample_rate: f64) -> Result<f64, Error> {
        let mut state = self.state.lock();
        state.tx_sample_rate = Some(sample_rate);
        if state.coupled_rates {
            state.applied_tx_rate_hz = Some(sample_rate);
            if state.rx_setup.is_some() {
                state.applied_rx_rate_hz = Some(sample_rate);
            }
        }
        drop(state);
        Ok(self.tx_sample_rate_hz())
    }
    fn set_tx_bandwidth(&mut self, bandwidth: usize) -> Result<(), Error> {
        self.state.lock().tx_bandwidth = Some(bandwidth);
        Ok(())
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
        let mut state = self.state.lock();
        let actual = state.applied_rx_rate_hz.unwrap_or(sample_rate_hz);
        if state.coupled_rates {
            state.applied_tx_rate_hz = Some(actual);
        }
        Ok(actual)
    }
    fn split(self: Box<Self>) -> Result<(Box<dyn RadioTx>, Option<Box<dyn RadioRx>>), Error> {
        let active = Arc::new(AtomicBool::new(false));
        Ok((
            Box::new(FakeTx {
                state: self.state.clone(),
                clock: self.clock.clone(),
            }),
            Some(Box::new(FakeRx {
                state: self.state,
                clock: self.clock,
                active,
                next_sample: 0,
                stream_start_tick: None,
            })),
        ))
    }
}

struct FakeTx {
    state: Arc<Mutex<FakeState>>,
    clock: Arc<FakeClock>,
}

impl RadioTx for FakeTx {
    fn capabilities(&self) -> sdr_hal::TxCapabilities {
        let capabilities = sdr_hal::TxCapabilities::from([sdr_hal::TxCapability::BurstEnd]);
        if self.supports_timed_tx() {
            capabilities
                .with(sdr_hal::TxCapability::TimedSamples)
                .with(sdr_hal::TxCapability::TimedActivation)
        } else {
            capabilities
        }
    }
    fn end_burst(&mut self) -> Result<(), Error> {
        self.state.lock().burst_ends += 1;
        Ok(())
    }

    fn tx_sample_rate_hz(&self) -> f64 {
        {
            let state = self.state.lock();
            state
                .applied_tx_rate_hz
                .unwrap_or(state.tx_sample_rate.unwrap_or_default())
        }
    }
    fn set_tx_frequency_hz(&mut self, frequency_hz: f64) -> Result<(), Error> {
        self.state.lock().tx_frequency = Some(frequency_hz as usize);
        Ok(())
    }
    fn supports_timed_tx(&self) -> bool {
        self.state.lock().timed_tx_supported.unwrap_or(true)
    }
    fn tick_rate(&self) -> u64 {
        self.state.lock().tick_rate.unwrap_or(INNER_TICK_RATE)
    }
    fn get_hardware_time(&self) -> Result<u64, Error> {
        Ok(self.clock.now_ticks())
    }
    fn transmit(&mut self, samples: &[Complex32]) -> Result<(), Error> {
        let delay = self.state.lock().tx_write_delay;
        if let Some(delay) = delay {
            std::thread::sleep(delay);
        }
        self.state.lock().transmits.push((None, samples.to_vec()));
        Ok(())
    }
    fn transmit_at(&mut self, samples: &[Complex32], tick: Option<u64>) -> Result<(), Error> {
        self.state.lock().transmits.push((tick, samples.to_vec()));
        Ok(())
    }
    fn enable_transmit(&mut self, enable: bool) -> Result<(), Error> {
        self.state.lock().transmit_enabled = Some((enable, None));
        Ok(())
    }
    fn enable_transmit_at(&mut self, enable: bool, tick: Option<u64>) -> Result<(), Error> {
        let mut state = self.state.lock();
        state.transmit_enabled = Some((enable, tick));
        state.enable_calls += 1;
        Ok(())
    }
}

struct FakeRx {
    state: Arc<Mutex<FakeState>>,
    clock: Arc<FakeClock>,
    active: Arc<AtomicBool>,
    next_sample: u64,
    stream_start_tick: Option<u64>,
}

fn ramp_value(n: u64) -> f32 {
    (n % 1000) as f32 / 1000.0
}

impl RadioRx for FakeRx {
    fn rx_sample_rate_hz(&self) -> f64 {
        let state = self.state.lock();
        state.applied_rx_rate_hz.unwrap_or_else(|| {
            state
                .rx_setup
                .as_ref()
                .map(|setup| setup.2)
                .unwrap_or_default()
        })
    }
    fn set_rx_frequency(&mut self, frequency_hz: f64) -> Result<(), Error> {
        self.state.lock().rx_frequency_hz = Some(frequency_hz);
        Ok(())
    }
    fn set_rx_gain(&mut self, gain_db: f64) -> Result<(), Error> {
        if let Some(setup) = &mut self.state.lock().rx_setup {
            setup.4 = Some(gain_db);
        }
        Ok(())
    }
    fn tick_rate(&self) -> u64 {
        let state = self.state.lock();
        state
            .rx_tick_rate
            .unwrap_or_else(|| state.tick_rate.unwrap_or(RX_TICK_RATE))
    }
    fn get_hardware_time(&self) -> Result<u64, Error> {
        Ok(self.clock.now_ticks())
    }
    fn rx_read(&mut self, buf: &mut [Complex32], _timeout_us: i64) -> Result<RxReadResult, Error> {
        if !self.active.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(1));
            return Ok(RxReadResult {
                samples_read: 0,
                time_ticks: self.clock.now_ticks(),
                overflow: false,
            });
        }
        {
            let mut state = self.state.lock();
            if state.stale_rx_blocks > 0 {
                state.stale_rx_blocks -= 1;
                let count = buf.len().min(RX_CHUNK);
                buf[..count].fill(Complex32::new(-1.0, 0.0));
                return Ok(RxReadResult {
                    samples_read: count,
                    time_ticks: 0,
                    overflow: false,
                });
            }
        }
        std::thread::sleep(Duration::from_micros(
            (RX_CHUNK as f64 / RX_RATE_HZ * 1_000_000.0) as u64,
        ));
        let start_tick = *self
            .stream_start_tick
            .get_or_insert_with(|| self.clock.now_ticks());
        let n = buf.len().min(RX_CHUNK);
        for (i, slot) in buf[..n].iter_mut().enumerate() {
            let sample = self.next_sample + i as u64;
            *slot = Complex32::new(ramp_value(sample), 0.0);
        }
        let time_ticks = start_tick
            + (self.next_sample as u128 * RX_TICK_RATE as u128 / RX_RATE_HZ as u128) as u64;
        self.next_sample += n as u64;
        Ok(RxReadResult {
            samples_read: n,
            time_ticks,
            overflow: false,
        })
    }
    fn rx_activate(&mut self, _time_ticks: Option<u64>) -> Result<(), Error> {
        self.state.lock().rx_activated = true;
        self.active.store(true, Ordering::Relaxed);
        Ok(())
    }
    fn stop(&mut self) -> Result<(), Error> {
        self.state.lock().rx_activated = false;
        self.active.store(false, Ordering::Relaxed);
        Ok(())
    }
}

fn start_server(state: Arc<Mutex<FakeState>>) -> (std::thread::JoinHandle<()>, SocketAddr) {
    start_server_with_calibration(
        state,
        RadioCalibration {
            rx_to_tx_delay_ns: Some(TX_SAMPLE_DELAY),
            tx_full_scale_power_estimate_dbm: Some(-6.0),
            rx_reference_dbm: Some(-39.0),
        },
    )
}

fn start_server_with_calibration(
    state: Arc<Mutex<FakeState>>,
    calibration: RadioCalibration,
) -> (std::thread::JoinHandle<()>, SocketAddr) {
    let radio = Box::new(FakeRadio {
        state,
        clock: Arc::new(FakeClock {
            start: Instant::now(),
        }),
    });
    let radio = Box::new(
        CalibratedRadio::new(radio, calibration)
            .unwrap()
            .with_rx_reference_gain_db(Some(30.0))
            .unwrap(),
    );
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("server runtime");
        rt.block_on(async move {
            let (tonic_ready_tx, tonic_ready_rx) = tokio::sync::oneshot::channel();
            let localhost = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0);
            let serve_fut = serve(
                radio,
                true,
                "fake".to_string(),
                RxDefaults {
                    antenna: "FAKE_RX".to_string(),
                    gain_db: Some(30.0),
                },
                None,
                localhost,
                localhost,
                Some(tonic_ready_tx),
                std::future::pending(),
            );
            tokio::pin!(serve_fut);
            tokio::select! {
                result = &mut serve_fut => panic!("server exited early: {result:?}"),
                ready = tonic_ready_rx => {
                    let (control_addr, _data_addr) = ready.expect("server ready");
                    ready_tx.send(control_addr).expect("report ready");
                    let _ = serve_fut.await;
                }
            }
        });
    });
    let control_addr = ready_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("server came up");
    (handle, control_addr)
}

#[test]
fn client_connects_from_multithreaded_tokio_runtime() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let (_server, control_addr) = start_server(state);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("client runtime");

    rt.block_on(async {
        let mut radio =
            NetworkRadio::connect(&control_addr.to_string(), NetworkRadioOptions::default())
                .expect("client connects from async runtime");
        radio
            .set_tx_frequency(881_520_000)
            .expect("control RPC from async runtime");
    });
}

#[test]
fn loopback_tx_rx_end_to_end() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let (_server, control_addr) = start_server(state.clone());

    let mut radio = Box::new(
        NetworkRadio::connect(&control_addr.to_string(), NetworkRadioOptions::default())
            .expect("client connects"),
    );
    assert_eq!(radio.rx_to_tx_delay_ns(), Some(TX_SAMPLE_DELAY));
    assert_eq!(radio.tx_full_scale_power_estimate_dbm(), Some(-6.0));
    assert_eq!(radio.rx_reference_dbm(), Some(-39.0));

    radio.set_tx_bandwidth(1_250_000).expect("bandwidth");
    radio
        .set_tx_sample_rate(TX_RATE_HZ as f64)
        .expect("sample rate");
    // Backends that clock in samples only know their tick rate once the rate is set.
    assert_eq!(radio.tick_rate(), INNER_TICK_RATE);
    assert!(state.lock().tx_frequency.is_none());
    radio
        .setup_rx(0, "", RX_RATE_HZ, 2_500_000.0, None)
        .expect("setup rx");
    {
        let state = state.lock();
        assert!(state.tx_frequency.is_none());
        assert_eq!(state.tx_sample_rate, Some(TX_RATE_HZ as f64));
        assert_eq!(state.tx_bandwidth, Some(1_250_000));
        let rx = state.rx_setup.as_ref().expect("rx setup reached inner");
        assert_eq!(rx.1, "FAKE_RX");
        assert_eq!(rx.4, Some(30.0));
    }

    let (mut tx, rx) = (radio as Box<dyn Radio>).split().expect("split");
    let mut rx = rx.expect("rx half");
    assert!(state.lock().rx_frequency_hz.is_none());
    tx.set_tx_frequency_hz(881_520_000.0).unwrap();
    rx.set_rx_frequency(836_520_000.0).unwrap();
    assert_eq!(state.lock().tx_frequency, Some(881_520_000));
    assert_eq!(state.lock().rx_frequency_hz, Some(836_520_000.0));
    assert_eq!(tx.rx_to_tx_delay_ns(), Some(TX_SAMPLE_DELAY));
    assert_eq!(tx.tx_full_scale_power_estimate_dbm(), Some(-6.0));
    assert_eq!(rx.rx_reference_dbm(), Some(-39.0));

    let t0 = tx.get_hardware_time().expect("time");
    std::thread::sleep(Duration::from_millis(5));
    let t1 = tx.get_hardware_time().expect("time");
    assert!(t1 > t0, "clock not advancing: {t0} {t1}");
    assert!(
        t1 - t0 >= INNER_TICK_RATE * 4 / 1_000,
        "clock too slow: {t0} {t1}"
    );
    assert!(
        t1 - t0 < INNER_TICK_RATE * 50 / 1_000,
        "clock too fast: {t0} {t1}"
    );

    let enable_lead = INNER_TICK_RATE / 10;
    tx.enable_transmit_at(true, Some(t1 + enable_lead))
        .expect("enable tx");

    let batch_ticks = TX_BLOCK_SAMPLES as u64 * INNER_TICK_RATE / TX_RATE_HZ;
    let start_ticks = t1 + INNER_TICK_RATE / 5;
    for batch in 0..2u64 {
        let samples: Vec<Complex32> = (0..TX_BLOCK_SAMPLES)
            .map(|i| {
                let v = ((batch * TX_BLOCK_SAMPLES as u64 + i as u64) % 500) as f32 / 500.0 - 0.5;
                Complex32::new(v, -v)
            })
            .collect();
        tx.transmit_at(&samples, Some(start_ticks + batch * batch_ticks))
            .expect("transmit");
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let total: usize = state
            .lock()
            .transmits
            .iter()
            .map(|(_, samples)| samples.len())
            .sum();
        if total >= 2 * TX_BLOCK_SAMPLES {
            break;
        }
        assert!(Instant::now() < deadline, "TX samples never arrived");
        std::thread::sleep(Duration::from_millis(10));
    }

    {
        let state = state.lock();
        let (enabled, enable_tick) = state.transmit_enabled.expect("enable reached inner");
        assert!(enabled);
        assert_eq!(
            state.transmits.len(),
            2,
            "each network TX block must become one driver call"
        );
        assert!(
            state
                .transmits
                .iter()
                .all(|(_, samples)| samples.len() == TX_BLOCK_SAMPLES)
        );
        let tick = enable_tick.expect("timed enable");
        assert_eq!(
            tick,
            t1 + enable_lead,
            "enable tick must reach the driver exactly"
        );

        let (first_tick, _) = &state.transmits[0];
        assert_eq!(
            first_tick.expect("timed transmit"),
            start_ticks,
            "first-sample tick must reach the driver exactly"
        );

        let flat: Vec<Complex32> = state
            .transmits
            .iter()
            .flat_map(|(_, samples)| samples.iter().copied())
            .collect();
        for (i, sample) in flat.iter().take(2 * TX_BLOCK_SAMPLES).enumerate() {
            let v = (i % 500) as f32 / 500.0 - 0.5;
            assert!(
                (sample.re - v).abs() < 2.0 / 32767.0,
                "sample {i}: {} vs {v}",
                sample.re
            );
            assert!((sample.im + v).abs() < 2.0 / 32767.0);
        }

        let ticks: Vec<u64> = state
            .transmits
            .iter()
            .map(|(tick, _)| tick.expect("timed"))
            .collect();
        let samples_per: Vec<usize> = state
            .transmits
            .iter()
            .map(|(_, samples)| samples.len())
            .collect();
        for i in 1..ticks.len() {
            let expected_delta =
                (samples_per[i - 1] as u128 * INNER_TICK_RATE as u128 / TX_RATE_HZ as u128) as u64;
            let delta = ticks[i] - ticks[i - 1];
            assert!(
                delta.abs_diff(expected_delta) <= 1,
                "packet {i}: delta {delta} vs {expected_delta}"
            );
        }
    }

    rx.rx_activate(None).expect("rx activate");
    assert!(state.lock().rx_activated);

    let mut collected: Vec<Complex32> = Vec::new();
    let mut reads: Vec<(u64, usize)> = Vec::new();
    let mut buf = vec![Complex32::default(); 12_288];
    let deadline = Instant::now() + Duration::from_secs(10);
    while collected.len() < 40_000 {
        assert!(Instant::now() < deadline, "RX samples never arrived");
        let result = rx.rx_read(&mut buf, 250_000).expect("rx read");
        if result.samples_read == 0 {
            continue;
        }
        assert!(!result.overflow, "unexpected RX discontinuity on loopback");
        reads.push((result.time_ticks, result.samples_read));
        collected.extend_from_slice(&buf[..result.samples_read]);
    }

    for (i, sample) in collected.iter().enumerate() {
        let expected = ramp_value(i as u64);
        assert!(
            (sample.re - expected).abs() < 2.0 / 32767.0,
            "rx sample {i}: {} vs {expected}",
            sample.re
        );
    }
    let (first_ticks, _) = reads[0];
    let mut offset = 0u64;
    for (time_ticks, count) in &reads {
        let expected =
            first_ticks + (offset as u128 * RX_TICK_RATE as u128 / RX_RATE_HZ as u128) as u64;
        assert_eq!(
            *time_ticks, expected,
            "read at offset {offset}: {time_ticks} vs {expected}"
        );
        offset += *count as u64;
    }

    rx.stop().expect("RX stop");
    assert!(!state.lock().rx_activated);
    tx.enable_transmit(false).expect("disable tx");
}

#[test]
fn composite_rate_tx_block_arrives_intact() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let (_server, control_addr) = start_server(state.clone());

    let mut radio = Box::new(
        NetworkRadio::connect(&control_addr.to_string(), NetworkRadioOptions::default())
            .expect("client connects"),
    ) as Box<dyn Radio>;
    radio
        .set_tx_bandwidth(COMPOSITE_BANDWIDTH_HZ)
        .expect("bandwidth");
    assert_eq!(
        radio
            .set_tx_sample_rate(COMPOSITE_TX_RATE_HZ as f64)
            .expect("sample rate"),
        COMPOSITE_TX_RATE_HZ as f64
    );
    radio
        .setup_rx(
            0,
            "",
            COMPOSITE_TX_RATE_HZ as f64,
            COMPOSITE_BANDWIDTH_HZ as f64,
            None,
        )
        .expect("setup rx");
    let (mut tx, rx) = radio.split().expect("split");
    let mut rx = rx.expect("composite reverse capture needs an RX half");
    rx.set_rx_frequency(COMPOSITE_RX_CENTER_HZ).unwrap();

    let t0 = tx.get_hardware_time().expect("time");
    let start_ticks = t0 + INNER_TICK_RATE / 5;
    let samples: Vec<Complex32> = (0..COMPOSITE_BLOCK_SAMPLES)
        .map(|i| {
            let v = (i % 500) as f32 / 500.0 - 0.5;
            Complex32::new(v, -v)
        })
        .collect();
    tx.transmit_at(&samples, Some(start_ticks))
        .expect("transmit");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        {
            let state = state.lock();
            if let Some((tick, received)) = state.transmits.first() {
                assert_eq!(received.len(), COMPOSITE_BLOCK_SAMPLES);
                assert_eq!(
                    tick.expect("timed transmit"),
                    start_ticks,
                    "first-sample tick must reach the driver exactly"
                );
                for (i, sample) in received.iter().enumerate() {
                    let v = (i % 500) as f32 / 500.0 - 0.5;
                    assert!(
                        (sample.re - v).abs() < 2.0 / 32767.0,
                        "sample {i}: {} vs {v}",
                        sample.re
                    );
                    assert!((sample.im + v).abs() < 2.0 / 32767.0);
                }
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "composite TX block never arrived"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let state = state.lock();
    assert_eq!(
        state.tx_sample_rate,
        Some(COMPOSITE_TX_RATE_HZ as f64),
        "server must apply the composite TX rate to the inner radio"
    );
    let rx_setup = state.rx_setup.as_ref().expect("rx setup reached inner");
    assert_eq!(state.rx_frequency_hz, Some(COMPOSITE_RX_CENTER_HZ));
    assert_eq!(rx_setup.2, COMPOSITE_TX_RATE_HZ as f64);
    assert_eq!(rx_setup.3, COMPOSITE_BANDWIDTH_HZ as f64);
}

#[test]
fn split_without_any_negotiated_stream_is_rejected() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let (_server, control_addr) = start_server(state);

    let radio = Box::new(
        NetworkRadio::connect(&control_addr.to_string(), NetworkRadioOptions::default())
            .expect("client connects"),
    ) as Box<dyn Radio>;
    let error = radio.split().err().expect("split must reject");
    assert!(error.to_string().contains("setup_rx"), "{error}");
}

#[test]
fn a_fractional_tx_rate_reaches_the_radio_exactly() {
    const FRACTIONAL_RATE_HZ: f64 = 3_250_000.0 / 3.0;
    let state = Arc::new(Mutex::new(FakeState::default()));
    let (_server, control_addr) = start_server(state.clone());
    let mut radio =
        NetworkRadio::connect(&control_addr.to_string(), NetworkRadioOptions::default())
            .expect("client connects");
    assert_eq!(
        radio.set_tx_sample_rate(FRACTIONAL_RATE_HZ).unwrap(),
        FRACTIONAL_RATE_HZ
    );
    assert_eq!(state.lock().tx_sample_rate, Some(FRACTIONAL_RATE_HZ));
}

#[test]
fn a_single_device_server_sets_no_ranges_of_its_own() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let (_server, control_addr) = start_server(state);
    let radio = NetworkRadio::connect(&control_addr.to_string(), NetworkRadioOptions::default())
        .expect("client connects");
    assert_eq!(radio.accepted_ranges(), Default::default());
    assert!(radio.supports_rx());
}

#[test]
fn receive_only_session_streams_rx_and_refuses_tx() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let (_server, control_addr) = start_server(state.clone());
    let mut radio = Box::new(
        NetworkRadio::connect(&control_addr.to_string(), NetworkRadioOptions::default()).unwrap(),
    );
    radio.setup_rx(0, "", RX_RATE_HZ, RX_RATE_HZ, None).unwrap();
    let (mut tx, rx) = radio.split().expect("an RX-only session opens");
    let mut rx = rx.expect("RX half");
    rx.rx_activate(None).unwrap();
    let mut samples = [Complex32::default(); 128];
    assert_eq!(rx.rx_read(&mut samples, 250_000).unwrap().samples_read, 128);

    assert!(tx.transmit(&samples).is_err());
    let error = tx
        .enable_transmit(true)
        .expect_err("no TX stream to enable");
    assert!(error.to_string().contains("no TX stream"), "{error}");
    assert!(state.lock().transmits.is_empty());
    assert_eq!(state.lock().enable_calls, 0);
    rx.stop().unwrap();
}

#[test]
fn device_clock_change_applies_and_clears_the_negotiated_rate() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let (_server, control_addr) = start_server(state.clone());

    let mut radio = Box::new(
        NetworkRadio::connect(&control_addr.to_string(), NetworkRadioOptions::default())
            .expect("client connects"),
    );
    radio
        .set_tx_sample_rate(TX_RATE_HZ as f64)
        .expect("sample rate");
    assert_eq!(radio.tick_rate(), INNER_TICK_RATE);

    let applied = radio.set_tick_rate(40_500_000).expect("device clock");
    assert_eq!(applied, 40_000_000, "the driver coerces to whole megahertz");
    assert_eq!(radio.tick_rate(), 40_000_000);
    assert_eq!(state.lock().tick_rate, Some(40_000_000));

    // The rate negotiated against the old clock is gone, so opening must ask again.
    let radio = radio as Box<dyn Radio>;
    let error = radio.split().err().expect("split must reject");
    assert!(error.to_string().contains("SetTxSampleRate"), "{error}");
}

#[test]
fn halves_that_disagree_on_the_tick_rate_are_rejected() {
    let state = Arc::new(Mutex::new(FakeState {
        rx_tick_rate: Some(INNER_TICK_RATE * 2),
        ..FakeState::default()
    }));
    let (_server, control_addr) = start_server(state.clone());

    let mut radio = Box::new(
        NetworkRadio::connect(&control_addr.to_string(), NetworkRadioOptions::default())
            .expect("client connects"),
    );
    radio
        .set_tx_sample_rate(TX_RATE_HZ as f64)
        .expect("sample rate");
    radio
        .setup_rx(0, "", RX_RATE_HZ, 2_500_000.0, None)
        .expect("setup rx");

    let radio = radio as Box<dyn Radio>;
    let error = radio.split().err().expect("split must reject");
    assert!(error.to_string().contains("must match"), "{error}");
}

#[test]
fn tcp_transport_delivers_whole_tx_blocks() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let (_server, control_addr) = start_server(state.clone());

    let mut radio = Box::new(
        NetworkRadio::connect(
            &control_addr.to_string(),
            NetworkRadioOptions {
                transport: Transport::Tcp,
                ..NetworkRadioOptions::default()
            },
        )
        .expect("client connects"),
    );
    radio
        .set_tx_sample_rate(TX_RATE_HZ as f64)
        .expect("sample rate");

    let (mut tx, _rx) = (radio as Box<dyn Radio>).split().expect("split");
    let t0 = tx.get_hardware_time().expect("time");
    tx.enable_transmit_at(true, Some(t0 + 100_000_000))
        .expect("enable tx");

    let batch_ns = TX_BLOCK_SAMPLES as u64 * 1_000_000_000 / TX_RATE_HZ;
    let start_ns = t0 + 200_000_000;
    for batch in 0..2u64 {
        let samples: Vec<Complex32> = (0..TX_BLOCK_SAMPLES)
            .map(|i| {
                let v = ((batch * TX_BLOCK_SAMPLES as u64 + i as u64) % 500) as f32 / 500.0 - 0.5;
                Complex32::new(v, -v)
            })
            .collect();
        tx.transmit_at(&samples, Some(start_ns + batch * batch_ns))
            .expect("transmit");
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let total: usize = state
            .lock()
            .transmits
            .iter()
            .map(|(_, samples)| samples.len())
            .sum();
        if total >= 2 * TX_BLOCK_SAMPLES {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "TX samples never arrived over TCP"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let state = state.lock();
    assert_eq!(
        state.transmits.len(),
        2,
        "each network TX block must become one driver call"
    );
    let flat: Vec<Complex32> = state
        .transmits
        .iter()
        .flat_map(|(_, samples)| samples.iter().copied())
        .collect();
    for (i, sample) in flat.iter().take(2 * TX_BLOCK_SAMPLES).enumerate() {
        let v = (i % 500) as f32 / 500.0 - 0.5;
        assert!(
            (sample.re - v).abs() < 2.0 / 32767.0,
            "sample {i}: {} vs {v}",
            sample.re
        );
        assert!((sample.im + v).abs() < 2.0 / 32767.0);
    }
}

#[test]
fn tcp_transport_carries_rx_as_well_as_tx() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let (_server, control_addr) = start_server(state.clone());

    let mut radio = NetworkRadio::connect(
        &control_addr.to_string(),
        NetworkRadioOptions {
            transport: Transport::Tcp,
            ..NetworkRadioOptions::default()
        },
    )
    .expect("client connects");
    radio
        .set_tx_sample_rate(TX_RATE_HZ as f64)
        .expect("sample rate");
    radio
        .setup_rx(0, "", RX_RATE_HZ, 2_500_000.0, None)
        .expect("setup rx");

    let (_tx, rx) = (Box::new(radio) as Box<dyn Radio>).split().expect("split");
    let mut rx = rx.expect("rx half");
    rx.rx_activate(None).expect("rx activate");

    let mut collected: Vec<Complex32> = Vec::new();
    let mut reads: Vec<(u64, usize)> = Vec::new();
    let mut buf = vec![Complex32::default(); 12_288];
    let deadline = Instant::now() + Duration::from_secs(10);
    while collected.len() < 40_000 {
        assert!(
            Instant::now() < deadline,
            "RX samples never arrived over TCP"
        );
        let result = rx.rx_read(&mut buf, 250_000).expect("rx read");
        if result.samples_read == 0 {
            continue;
        }
        assert!(!result.overflow, "unexpected RX discontinuity on loopback");
        reads.push((result.time_ticks, result.samples_read));
        collected.extend_from_slice(&buf[..result.samples_read]);
    }

    // A framing error would misalign the ramp rather than drop the stream.
    for (i, sample) in collected.iter().enumerate() {
        let expected = ramp_value(i as u64);
        assert!(
            (sample.re - expected).abs() < 2.0 / 32767.0,
            "rx sample {i}: {} vs {expected}",
            sample.re
        );
    }
    let (first_ticks, _) = reads[0];
    let mut offset = 0u64;
    for (time_ticks, count) in &reads {
        let expected =
            first_ticks + (offset as u128 * RX_TICK_RATE as u128 / RX_RATE_HZ as u128) as u64;
        assert_eq!(
            *time_ticks, expected,
            "read at offset {offset}: {time_ticks} vs {expected}"
        );
        offset += *count as u64;
    }

    rx.stop().expect("RX stop");
    assert!(!state.lock().rx_activated);
}

#[test]
fn silent_client_has_transmit_disabled_by_server() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let (_server, control_addr) = start_server(state.clone());

    let mut radio = Box::new(
        NetworkRadio::connect(&control_addr.to_string(), NetworkRadioOptions::default())
            .expect("client connects"),
    ) as Box<dyn Radio>;
    radio
        .set_tx_sample_rate(TX_RATE_HZ as f64)
        .expect("set tx sample rate");
    let (mut tx, _rx) = radio.split().expect("split");
    tx.enable_transmit(true).expect("enable tx");
    assert_eq!(state.lock().transmit_enabled, Some((true, None)));

    let deadline = Instant::now() + Duration::from_secs(5);
    while state.lock().transmit_enabled != Some((false, None)) {
        assert!(
            Instant::now() < deadline,
            "server never disabled TX for a silent client"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_tcp_client_leaving_stops_transmit_without_waiting_for_its_backlog() {
    let state = Arc::new(Mutex::new(FakeState {
        tx_write_delay: Some(Duration::from_millis(20)),
        ..FakeState::default()
    }));
    let (_server, control_addr) = start_server(state.clone());
    let options = NetworkRadioOptions {
        transport: Transport::Tcp,
        ..NetworkRadioOptions::default()
    };
    let mut radio = Box::new(NetworkRadio::connect(&control_addr.to_string(), options).unwrap());
    radio.set_tx_sample_rate(TX_RATE_HZ as f64).unwrap();
    let (mut tx, _rx) = radio.split().unwrap();
    tx.enable_transmit(true).unwrap();
    // A slow driver leaves these queued when the client leaves.
    let block = vec![Complex32::new(0.25, 0.0); TX_BLOCK_SAMPLES];
    for _ in 0..40 {
        tx.transmit(&block).unwrap();
    }
    tx.enable_transmit(false).unwrap();
    drop(tx);
    drop(_rx);
    let left = Instant::now();
    while !matches!(state.lock().transmit_enabled, Some((false, _)))
        && left.elapsed() < Duration::from_secs(2)
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        left.elapsed() < Duration::from_millis(300),
        "transmit stayed on for {:?} after the client left",
        left.elapsed()
    );
}

#[test]
fn untimed_transmit_uses_inner_untimed_path() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let (_server, control_addr) = start_server(state.clone());

    let mut radio = Box::new(
        NetworkRadio::connect(&control_addr.to_string(), NetworkRadioOptions::default())
            .expect("client connects"),
    ) as Box<dyn Radio>;
    radio
        .set_tx_sample_rate(TX_RATE_HZ as f64)
        .expect("set tx sample rate");
    let (mut tx, _rx) = radio.split().expect("split");

    let samples = vec![Complex32::new(0.25, -0.25); 100];
    tx.transmit(&samples).expect("untimed transmit");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        {
            let state = state.lock();
            if let Some((tick, samples)) = state.transmits.first() {
                assert!(tick.is_none(), "untimed transmit must stay untimed");
                assert_eq!(samples.len(), 100);
                break;
            }
        }
        assert!(Instant::now() < deadline, "untimed samples never arrived");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn get_hardware_time_tracks_server_clock() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let (_server, control_addr) = start_server(state);

    let mut radio = Box::new(
        NetworkRadio::connect(&control_addr.to_string(), NetworkRadioOptions::default())
            .expect("client connects"),
    ) as Box<dyn Radio>;
    radio
        .set_tx_sample_rate(TX_RATE_HZ as f64)
        .expect("set tx sample rate");
    let (tx, _rx) = radio.split().expect("split");

    let mut last = 0u64;
    for _ in 0..1000 {
        let now = tx.get_hardware_time().expect("time");
        assert!(now >= last);
        last = now;
    }
}

#[test]
fn remote_rx_reports_coerced_rate_and_reconnect_keeps_it() {
    const REQUESTED_HZ: f64 = 2_000_000.0;
    const APPLIED_HZ: f64 = 1_999_999.25;
    let state = Arc::new(Mutex::new(FakeState {
        applied_rx_rate_hz: Some(APPLIED_HZ),
        ..Default::default()
    }));
    let (_server, address) = start_server(state);
    let mut radio =
        NetworkRadio::connect(&address.to_string(), NetworkRadioOptions::default()).unwrap();
    assert_eq!(
        radio
            .setup_rx(0, "", REQUESTED_HZ, REQUESTED_HZ, None)
            .unwrap(),
        APPLIED_HZ
    );
    radio.set_tx_sample_rate(REQUESTED_HZ).unwrap();
    assert_eq!(radio.rx_sample_rate_hz(), Some(APPLIED_HZ));
    assert_eq!(radio.tx_sample_rate_hz(), REQUESTED_HZ);
    let (tx, rx) = Box::new(radio).split().unwrap();
    assert_eq!(tx.tx_sample_rate_hz(), REQUESTED_HZ);
    assert_eq!(rx.unwrap().rx_sample_rate_hz(), APPLIED_HZ);
    let mut reconnect =
        NetworkRadio::connect(&address.to_string(), NetworkRadioOptions::default()).unwrap();
    assert_eq!(
        reconnect
            .setup_rx(0, "", REQUESTED_HZ, REQUESTED_HZ, None)
            .unwrap(),
        APPLIED_HZ
    );
}

#[test]
fn remote_backend_rejects_unsupported_timed_tx_before_sending() {
    const SAMPLE_RATE_HZ: usize = 2_000_000;
    const TIME_NS: u64 = 1_000_000;
    let state = Arc::new(Mutex::new(FakeState {
        timed_tx_supported: Some(false),
        ..Default::default()
    }));
    let (_server, address) = start_server(state.clone());
    let mut radio =
        NetworkRadio::connect(&address.to_string(), NetworkRadioOptions::default()).unwrap();
    assert!(!radio.supports_timed_tx());
    radio.set_tx_sample_rate(SAMPLE_RATE_HZ as f64).unwrap();
    let (mut tx, _) = Box::new(radio).split().unwrap();
    assert!(
        tx.transmit_at(&[Complex32::new(0.25, 0.0)], Some(TIME_NS))
            .is_err()
    );
    assert!(state.lock().transmits.is_empty());
}

#[test]
fn daemon_shutdown_stops_tx_and_deactivates_rx_before_returning() {
    const SAMPLE_RATE_HZ: usize = 2_000_000;
    let state = Arc::new(Mutex::new(FakeState::default()));
    let server_state = state.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server = std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (ready, ready_receiver) = tokio::sync::oneshot::channel();
            let radio = Box::new(FakeRadio { state: server_state, clock: Arc::new(FakeClock { start: Instant::now() }) });
            let localhost = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0);
            let service = serve(radio, true, "fake".into(), RxDefaults::default(), None, localhost, localhost, Some(ready), async { let _ = shutdown_rx.await; });
            tokio::pin!(service);
            tokio::select! {
                result = &mut service => panic!("server exited before ready: {result:?}"),
                ready_result = ready_receiver => { ready_tx.send(ready_result.unwrap().0).unwrap(); }
            }
            service.await.unwrap();
        });
    });
    let address = ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let mut radio =
        NetworkRadio::connect(&address.to_string(), NetworkRadioOptions::default()).unwrap();
    radio.set_tx_sample_rate(SAMPLE_RATE_HZ as f64).unwrap();
    radio
        .setup_rx(0, "", SAMPLE_RATE_HZ as f64, SAMPLE_RATE_HZ as f64, None)
        .unwrap();
    let (mut tx, rx) = Box::new(radio).split().unwrap();
    let mut rx = rx.unwrap();
    rx.rx_activate(None).unwrap();
    tx.enable_transmit(true).unwrap();
    assert_eq!(state.lock().transmit_enabled, Some((true, None)));
    shutdown_tx.send(()).unwrap();
    drop(tx);
    drop(rx);
    server.join().unwrap();
    assert_eq!(state.lock().transmit_enabled, Some((false, None)));
    assert!(!state.lock().rx_activated);
}

#[test]
fn factory_preserves_server_calibration_and_applies_local_overrides() {
    use sdr_hal::config::RadioConfig;

    const SAMPLE_RATE_HZ: usize = 2_000_000;
    const LOCAL_DELAY_NS: i64 = 125;
    let state = Arc::new(Mutex::new(FakeState::default()));
    let (_server, address) = start_server(state.clone());
    let config: RadioConfig = serde_json::from_value(serde_json::json!({
        "kind": "network", "addr": address.to_string(),
        "rx_to_tx_delay_ns": LOCAL_DELAY_NS,
        "tx_sample_rate_hz": SAMPLE_RATE_HZ,
        "rx_sample_rate_hz": SAMPLE_RATE_HZ, "rx_bandwidth_hz": SAMPLE_RATE_HZ
    }))
    .unwrap();
    let opened = config.open().unwrap();
    assert_eq!(opened.tx_sample_rate_hz(), SAMPLE_RATE_HZ as f64);
    assert_eq!(opened.rx_sample_rate_hz(), Some(SAMPLE_RATE_HZ as f64));
    let radio = opened;
    assert!(state.lock().tx_frequency.is_none());
    assert!(state.lock().rx_frequency_hz.is_none());
    assert_eq!(radio.rx_to_tx_delay_ns(), Some(LOCAL_DELAY_NS));
    assert_eq!(radio.tx_full_scale_power_estimate_dbm(), Some(-6.0));
    assert_eq!(radio.rx_reference_dbm(), Some(-39.0));
    let (mut tx, rx) = radio.split().unwrap();
    let mut rx = rx.unwrap();
    assert_eq!(tx.tx_sample_rate_hz(), SAMPLE_RATE_HZ as f64);
    assert_eq!(rx.rx_sample_rate_hz(), SAMPLE_RATE_HZ as f64);
    tx.set_tx_frequency_hz(100_000_000.0).unwrap();
    rx.set_rx_frequency(90_000_000.0).unwrap();
    assert_eq!(state.lock().tx_frequency, Some(100_000_000));
    assert_eq!(state.lock().rx_frequency_hz, Some(90_000_000.0));
    assert_eq!(tx.rx_to_tx_delay_ns(), Some(LOCAL_DELAY_NS));
    assert_eq!(tx.tx_full_scale_power_estimate_dbm(), Some(-6.0));
    assert_eq!(rx.rx_reference_dbm(), Some(-39.0));
}

#[test]
fn client_connects_and_drops_inside_current_thread_tokio_runtime() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let (_server, address) = start_server(state);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut radio =
                NetworkRadio::connect(&address.to_string(), NetworkRadioOptions::default())
                    .unwrap();
            radio.set_tx_sample_rate(TX_RATE_HZ as f64).unwrap();
            let (mut tx, _) = Box::new(radio).split().unwrap();
            tx.enable_transmit(true).unwrap();
            tx.stop().unwrap();
        });
}
#[test]
fn remote_fractional_tx_rate_survives_split_and_reconnect() {
    const APPLIED: f64 = 1_999_999.25;
    let state = Arc::new(Mutex::new(FakeState {
        applied_tx_rate_hz: Some(APPLIED),
        ..Default::default()
    }));
    let (_server, address) = start_server(state);
    let mut radio =
        NetworkRadio::connect(&address.to_string(), NetworkRadioOptions::default()).unwrap();
    assert_eq!(radio.set_tx_sample_rate(2_000_000.0).unwrap(), APPLIED);
    assert_eq!(radio.tx_sample_rate_hz(), APPLIED);
    let (tx, _) = Box::new(radio).split().unwrap();
    assert_eq!(tx.tx_sample_rate_hz(), APPLIED);
    let mut reconnect =
        NetworkRadio::connect(&address.to_string(), NetworkRadioOptions::default()).unwrap();
    reconnect.set_tx_sample_rate(2_000_000.0).unwrap();
    assert_eq!(reconnect.tx_sample_rate_hz(), APPLIED);
}
#[test]
fn remote_gain_changes_invalidate_and_restore_measured_reference() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let (_server, address) = start_server(state);
    let mut radio =
        NetworkRadio::connect(&address.to_string(), NetworkRadioOptions::default()).unwrap();
    radio.set_tx_sample_rate(TX_RATE_HZ as f64).unwrap();
    radio
        .setup_rx(0, "", RX_RATE_HZ, RX_RATE_HZ, Some(30.0))
        .unwrap();
    let (_, rx) = Box::new(radio).split().unwrap();
    let mut rx = rx.unwrap();
    assert_eq!(rx.rx_reference_dbm(), Some(-39.0));
    rx.set_rx_gain(20.0).unwrap();
    assert_eq!(rx.rx_reference_dbm(), None);
    rx.set_rx_gain(30.0).unwrap();
    assert_eq!(rx.rx_reference_dbm(), Some(-39.0));
}
#[test]
fn remote_burst_metadata_keeps_activation_between_bursts() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let (_server, address) = start_server(state.clone());
    let mut radio = NetworkRadio::connect(
        &address.to_string(),
        NetworkRadioOptions {
            transport: Transport::Tcp,
            samples_per_packet: 2,
            ..Default::default()
        },
    )
    .unwrap();
    radio.set_tx_sample_rate(TX_RATE_HZ as f64).unwrap();
    let (mut tx, _) = Box::new(radio).split().unwrap();
    for _ in 0..2 {
        tx.enable_transmit(true).unwrap();
        tx.write_all(
            &[Complex32::new(0.25, 0.0); 8],
            sdr_hal::TxMetadata {
                schedule: sdr_hal::Schedule::Immediate,
                end_of_burst: true,
            },
        )
        .unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while state.lock().burst_ends < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    let state = state.lock();
    assert_eq!(state.burst_ends, 2);
    assert_eq!(state.enable_calls, 1);
    assert_eq!(
        state
            .transmits
            .iter()
            .map(|(_, samples)| samples.len())
            .sum::<usize>(),
        16
    );
    drop(state);
    tx.stop().unwrap();
}

#[test]
fn shared_clock_rate_changes_refresh_both_remote_readbacks() {
    let state = Arc::new(Mutex::new(FakeState {
        coupled_rates: true,
        ..Default::default()
    }));
    let (_server, address) = start_server(state);
    let mut radio =
        NetworkRadio::connect(&address.to_string(), NetworkRadioOptions::default()).unwrap();
    radio.set_tx_sample_rate(2_000_000.0).unwrap();
    radio
        .setup_rx(0, "", 1_000_000.0, 1_000_000.0, None)
        .unwrap();
    assert_eq!(radio.tx_sample_rate_hz(), 1_000_000.0);
    assert_eq!(radio.rx_sample_rate_hz(), Some(1_000_000.0));
    radio.set_tx_sample_rate(4_000_000.0).unwrap();
    assert_eq!(radio.tx_sample_rate_hz(), 4_000_000.0);
    assert_eq!(radio.rx_sample_rate_hz(), Some(4_000_000.0));
    let (tx, rx) = Box::new(radio).split().unwrap();
    assert_eq!(tx.tx_sample_rate_hz(), 4_000_000.0);
    assert_eq!(rx.unwrap().rx_sample_rate_hz(), 4_000_000.0);
}

#[test]
fn inherited_remote_reference_is_managed_by_the_backend_after_gain_changes() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let (_server, address) = start_server(state);
    let config: sdr_hal::config::RadioConfig = serde_json::from_value(serde_json::json!({
        "kind": "network", "addr": address.to_string(), "rx_gain_db": 20.0,
        "tx_sample_rate_hz": TX_RATE_HZ, "rx_sample_rate_hz": RX_RATE_HZ as usize,
        "rx_bandwidth_hz": RX_RATE_HZ as usize
    }))
    .unwrap();
    let radio = config.open().unwrap();
    assert_eq!(radio.rx_reference_dbm(), None);
    let (_, rx) = radio.split().unwrap();
    let mut rx = rx.unwrap();
    rx.set_rx_gain(30.0).unwrap();
    assert_eq!(rx.rx_reference_dbm(), Some(-39.0));
}
#[test]
fn invalidated_reference_override_can_be_restored_after_split() {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let inner = Box::new(FakeRadio {
        state,
        clock: Arc::new(FakeClock {
            start: Instant::now(),
        }),
    });
    let mut radio = CalibratedRadio::new(
        inner,
        RadioCalibration {
            rx_reference_dbm: Some(-17.0),
            ..Default::default()
        },
    )
    .unwrap()
    .with_rx_reference_gain_db(Some(30.0))
    .unwrap();
    radio
        .setup_rx(0, "FAKE_RX", RX_RATE_HZ, RX_RATE_HZ, Some(20.0))
        .unwrap();
    assert_eq!(radio.rx_reference_dbm(), None);
    let (_, rx) = Box::new(radio).split().unwrap();
    let mut rx = rx.unwrap();
    assert_eq!(rx.rx_reference_dbm(), None);
    rx.set_rx_gain(30.0).unwrap();
    assert_eq!(rx.rx_reference_dbm(), Some(-17.0));
}

#[test]
fn activation_discards_samples_buffered_before_the_stream_started() {
    let state = Arc::new(Mutex::new(FakeState {
        stale_rx_blocks: 3,
        ..FakeState::default()
    }));
    let (_server, control_addr) = start_server(state.clone());
    let mut radio = Box::new(
        NetworkRadio::connect(&control_addr.to_string(), NetworkRadioOptions::default()).unwrap(),
    );
    radio.set_tx_sample_rate(TX_RATE_HZ as f64).unwrap();
    radio.setup_rx(0, "", RX_RATE_HZ, RX_RATE_HZ, None).unwrap();
    let (tx, rx) = radio.split().unwrap();
    let mut rx = rx.unwrap();
    let before_activation_ns = tx.get_hardware_time().unwrap();
    rx.rx_activate(None).unwrap();
    let mut samples = [Complex32::default(); 128];
    let result = rx.rx_read(&mut samples, 250_000).unwrap();
    assert_eq!(result.samples_read, samples.len());
    assert!(result.time_ticks >= before_activation_ns);
    assert!(samples.iter().all(|sample| sample.re >= 0.0));
    assert_eq!(state.lock().stale_rx_blocks, 0);
    rx.stop().unwrap();
}
