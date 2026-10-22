#![allow(clippy::result_large_err)]

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::Radio;
use crate::network::proto::radio_control_service_server::{
    RadioControlService, RadioControlServiceServer,
};
use crate::network::{proto, wire};

use crate::error::Error;
use log::{info, warn};
use parking_lot::Mutex;
use tokio::sync::oneshot;
use tonic::{Request, Response, Status};

use super::bridge::{RxBridge, RxCommand, TxBridge, TxCommand};
use super::composite::{ClientRegistry, CompositeConfig, Direction, RxFanout, TxGrid};
use super::control::{ClientWatch, WatchedStream};
use super::sessions::{Session, Sessions};

/// RX settings used when a control request omits antenna or gain. The default is empty antenna and no gain override.
#[derive(Clone, Debug, Default)]
pub struct RxDefaults {
    /// Fallback native RX port name for an empty requested antenna.
    pub antenna: String,
    /// Fallback RX gain in dB when the request omits gain. `None` leaves gain unchanged.
    pub gain_db: Option<f64>,
}

pub use crate::RadioCalibration;

#[derive(Clone, Debug, Default, PartialEq)]
struct RecordedSettings {
    tx_frequency_hz: Option<u64>,
    tx_sample_rate_hz: Option<f64>,
    actual_tx_sample_rate_hz: Option<f64>,
    tx_bandwidth_hz: Option<u64>,
    tx_lo_offset_hz: Option<i64>,
    rx: Option<proto::SetupRxRequest>,
    actual_rx_sample_rate_hz: Option<f64>,
    tick_rate: u64,
}

struct IdleState {
    radio: Box<dyn Radio>,
    recorded: RecordedSettings,
}

struct RunningState {
    tx_thread: std::thread::JoinHandle<()>,
    rx_thread: Option<std::thread::JoinHandle<()>>,
    tx_cmds: crossbeam_channel::Sender<TxCommand>,
    rx_cmds: Option<crossbeam_channel::Sender<RxCommand>>,
    samples_per_packet: usize,
    recorded: RecordedSettings,
}

enum State {
    Idle(IdleState),
    Running(RunningState),
    Unavailable,
}

struct Shared {
    state: Mutex<State>,
    sessions: Arc<Sessions>,
    /// Composite client settings, present only when the operator configured a shared
    /// device stream.
    composite: Option<Arc<Mutex<ClientRegistry>>>,
    data_socket: UdpSocket,
    data_listener: Arc<std::net::TcpListener>,
    data_port: u16,
    rx_supported: bool,
    tx_capabilities: crate::TxCapabilities,
    backend: String,
    rx_defaults: RxDefaults,
    radio_calibration: RadioCalibration,
    rx_reference_bits: AtomicU32,
    rx_sample_rate_hz: Arc<AtomicU64>,
    /// Settable TX gain in dB, absent when clients cannot set it.
    tx_gain_range_db: Option<(f64, f64)>,
    /// Applied TX gain in dB as `f64` bits, NaN when unknown.
    tx_gain_bits: AtomicU64,
    /// TX power estimate at the applied gain as `f32` bits, NaN when unknown.
    tx_estimate_bits: AtomicU32,
}

/// gRPC control implementation created by [`serve`]. Sessions own the split radio streams.
pub struct RadioControlSvc {
    shared: Arc<Shared>,
}

fn internal(e: impl Into<Error>) -> Status {
    let e = e.into();
    use crate::error::ErrorKind;
    let code = match e.kind() {
        ErrorKind::InvalidSetting | ErrorKind::Protocol => tonic::Code::InvalidArgument,
        ErrorKind::Unsupported => tonic::Code::Unimplemented,
        ErrorKind::Timeout => tonic::Code::DeadlineExceeded,
        ErrorKind::Disconnected => tonic::Code::Unavailable,
        ErrorKind::Late => tonic::Code::FailedPrecondition,
        _ => tonic::Code::Internal,
    };
    Status::new(code, e.to_string())
}

fn new_session_id() -> u32 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    nanos ^ std::process::id()
}

impl Shared {
    fn apply_setting<T: PartialEq + Copy + std::fmt::Debug>(
        &self,
        name: &str,
        value: T,
        field: impl Fn(&mut RecordedSettings) -> &mut Option<T>,
        apply: impl FnOnce(&mut dyn Radio) -> Result<(), Error>,
    ) -> Result<(), Status> {
        let mut state = self.state.lock();
        match &mut *state {
            State::Idle(idle) => {
                apply(idle.radio.as_mut()).map_err(internal)?;
                *field(&mut idle.recorded) = Some(value);
                Ok(())
            }
            State::Running(running) => {
                if *field(&mut running.recorded) == Some(value) {
                    Ok(())
                } else {
                    Err(Status::failed_precondition(format!(
                        "cannot change {name} while a session is active"
                    )))
                }
            }
            State::Unavailable => Err(Status::unavailable("radio unavailable")),
        }
    }

    fn tx_command(&self, cmd: TxCommand) -> Result<(), Status> {
        let state = self.state.lock();
        match &*state {
            State::Running(running) => running
                .tx_cmds
                .send(cmd)
                .map_err(|_| Status::unavailable("radio TX bridge stopped")),
            _ => Err(Status::failed_precondition("no active session")),
        }
    }

    fn rx_command(&self, cmd: RxCommand) -> Result<(), Status> {
        let state = self.state.lock();
        match &*state {
            State::Running(running) => match &running.rx_cmds {
                Some(rx_cmds) => rx_cmds
                    .send(cmd)
                    .map_err(|_| Status::unavailable("radio RX bridge stopped")),
                None => Err(Status::failed_precondition("radio has no RX chain")),
            },
            _ => Err(Status::failed_precondition("no active session")),
        }
    }
}

/// Converts a request's floating-point Hz to whole Hz, which composite arithmetic needs.
fn whole_hz(value: f64, name: &str) -> Result<u64, Status> {
    if !value.is_finite() || value < 0.0 {
        return Err(Status::invalid_argument(format!(
            "{name} {value} is not a usable frequency"
        )));
    }
    let rounded = value.round();
    if (value - rounded).abs() > 1e-3 {
        return Err(Status::invalid_argument(format!(
            "composite mode needs {name} in whole Hz, and {value} is not"
        )));
    }
    Ok(rounded as u64)
}

/// Identifies a composite client by its control connection, since its rate, carrier and
/// bandwidth all arrive before it has a session token.
fn client_key<T>(request: &Request<T>) -> Result<SocketAddr, Status> {
    request.remote_addr().ok_or_else(|| {
        Status::failed_precondition(
            "composite mode identifies a client by its control connection, and this request has no peer address",
        )
    })
}

/// Logs a device setting a composite client cannot own, and keeps the client running.
///
/// Refusing would stop a client that sets gain or bandwidth at startup, so the operator
/// value stands and the log says so.
fn ignored_device_setting(name: &str, requested: impl std::fmt::Display) {
    warn!(
        "radiod: composite mode owns {name} for the whole device, so the requested {requested} is not applied. Set it in the radio configuration instead"
    );
}

impl RadioControlSvc {
    fn composite(&self) -> Option<&Mutex<ClientRegistry>> {
        self.shared.composite.as_deref()
    }

    /// Grid slot a client's TX packets feed, absent until its session opens.
    fn composite_tx_slot(&self, key: SocketAddr) -> Option<usize> {
        self.composite()?.lock().get(key)?.tx_slot
    }

    /// Opens one client's composite session, starting the stream workers for the first.
    ///
    /// Slots are added by command rather than through a shared lock, because the grid and
    /// the fan-out are owned by the worker threads that drive them.
    async fn composite_open_session(
        &self,
        key: SocketAddr,
        requested: usize,
    ) -> Result<proto::OpenSessionResponse, Status> {
        let registry = self.composite().expect("composite mode");
        let (tx_plan, rx_plan) = registry.lock().plan(key).map_err(internal)?;
        let samples_per_packet = requested.clamp(1, wire::MAX_SAMPLES_PER_PACKET);
        let (device_rate_hz, gain) = {
            let guard = registry.lock();
            (guard.config().device_rate_hz, guard.config().tx_scale())
        };
        let shared = &self.shared;

        let (tx_cmds, rx_cmds) = {
            let mut state = shared.state.lock();
            if matches!(&*state, State::Idle(_)) {
                self.composite_start(&mut state, samples_per_packet)?;
            }
            match &*state {
                State::Running(running) => (running.tx_cmds.clone(), running.rx_cmds.clone()),
                State::Idle(_) => {
                    return Err(Status::internal("composite start left the radio idle"));
                }
                State::Unavailable => return Err(Status::unavailable("radio unavailable")),
            }
        };

        let tx_slot = match tx_plan {
            Some(plan) => {
                let sample_rate_hz =
                    device_rate_hz as f64 * plan.denominator as f64 / plan.ratio as f64;
                let (reply, reply_rx) = oneshot::channel();
                tx_cmds
                    .send(TxCommand::AddSlot {
                        plan,
                        gain,
                        sample_rate_hz,
                        reply,
                    })
                    .map_err(|_| Status::unavailable("radio TX bridge stopped"))?;
                Some(await_reply(reply_rx).await?)
            }
            None => None,
        };

        let rx_slot = match rx_plan {
            Some(plan) => {
                let Some(rx_cmds) = rx_cmds else {
                    return Err(Status::failed_precondition("radio has no RX chain"));
                };
                let sample_rate_hz =
                    device_rate_hz as f64 * plan.denominator as f64 / plan.ratio as f64;
                let (reply, reply_rx) = oneshot::channel();
                rx_cmds
                    .send(RxCommand::AddSlot {
                        plan,
                        sample_rate_hz,
                        reply,
                    })
                    .map_err(|_| Status::unavailable("radio RX bridge stopped"))?;
                Some(await_reply(reply_rx).await?)
            }
            None => None,
        };

        let session_id = new_session_id();
        shared.sessions.insert(Session {
            session_id,
            peer: Arc::new(Mutex::new(None)),
            tx_slot,
            rx_slot,
        });
        let (evicted, attached) = {
            let mut guard = registry.lock();
            let evicted = guard.open_session(key, session_id, tx_slot, rx_slot);
            (evicted, guard.len())
        };
        if evicted > 0 {
            info!(
                "radiod: {evicted} composite client(s) dropped, their slot was released and reused"
            );
        }
        info!("radiod: {attached} composite client(s) attached");
        info!(
            "radiod: composite session {session_id:#010x} open for {key}, TX slot {tx_slot:?}, RX slot {rx_slot:?}, samples_per_packet={samples_per_packet}"
        );
        Ok(proto::OpenSessionResponse {
            session_id,
            data_port: shared.data_port as u32,
            samples_per_packet: samples_per_packet as u32,
        })
    }

    fn composite_start(&self, state: &mut State, samples_per_packet: usize) -> Result<(), Status> {
        let config = *self.composite().expect("composite mode").lock().config();
        let block_samples = config.grid_block_samples().map_err(internal)?;
        let State::Idle(mut idle) = std::mem::replace(state, State::Unavailable) else {
            return Err(Status::failed_precondition("radio is not idle"));
        };
        // The shared stream is the operator's, so the daemon configures the device from
        // the composite plan rather than from any one client. A client's own rate and
        // frequency name its slice within that stream.
        let tx_center = usize::try_from(config.tx_center_hz).map_err(|_| {
            Status::invalid_argument("composite.tx_center_hz exceeds platform usize")
        })?;
        idle.radio
            .set_tx_sample_rate(config.device_rate_hz as f64)
            .map_err(internal)?;
        idle.radio.set_tx_frequency(tx_center).map_err(internal)?;
        if self.shared.rx_supported && idle.radio.rx_sample_rate_hz().is_none() {
            idle.radio
                .setup_rx(
                    0,
                    &self.shared.rx_defaults.antenna,
                    config.device_rate_hz as f64,
                    config.device_rate_hz as f64,
                    self.shared.rx_defaults.gain_db,
                )
                .map_err(internal)?;
        }
        // Read back what the driver settled on. A coerced rate would move every carrier
        // offset, and the daemon binary checks this at startup, but an embedder calling
        // serve directly reaches the device only here.
        config
            .verify_device(idle.radio.as_ref())
            .map_err(internal)?;
        let mut recorded = idle.recorded;
        let (tx_half, rx_half) = match idle.radio.split() {
            Ok(halves) => halves,
            Err(e) => {
                warn!("radiod: radio split failed: {e}");
                return Err(internal(e));
            }
        };
        let tick_rate = tx_half.tick_rate();
        if tick_rate == 0 {
            return Err(Status::failed_precondition(
                "stream clock rate must be positive",
            ));
        }
        if let Some(rx) = rx_half.as_ref()
            && rx.tick_rate() != tick_rate
        {
            return Err(Status::failed_precondition(format!(
                "TX clock {tick_rate} Hz and RX clock {} Hz must match",
                rx.tick_rate()
            )));
        }
        recorded.tick_rate = tick_rate;

        let grid =
            TxGrid::new(config.device_rate_hz, tick_rate, block_samples).map_err(internal)?;
        let shared = &self.shared;
        let (tx_cmds, tx_cmd_rx) = crossbeam_channel::unbounded();
        let tx_bridge = TxBridge {
            sessions: shared.sessions.clone(),
            tick_rate,
            slot_rates: Vec::new(),
            tcp_listener: shared.data_listener.clone(),
            grid: Some(grid),
            composite_deadline: std::time::Duration::from_secs_f64(config.tx_deadline_ms / 1000.0),
            device_rate_hz: config.device_rate_hz,
        };
        let tx_socket = shared.data_socket.try_clone().map_err(internal)?;
        let tx_thread = std::thread::Builder::new()
            .name("radiod-tx".into())
            .spawn(move || tx_bridge.run(tx_half, tx_socket, tx_cmd_rx))
            .map_err(internal)?;

        let (rx_cmds, rx_thread) = match rx_half {
            Some(mut rx_half) => {
                rx_half
                    .set_rx_frequency(config.rx_center_hz as f64)
                    .map_err(internal)?;
                let fanout = RxFanout::new(config.device_rate_hz, tick_rate).map_err(internal)?;
                let (rx_cmds, rx_cmd_rx) = crossbeam_channel::unbounded();
                let rx_bridge = RxBridge {
                    sessions: shared.sessions.clone(),
                    tick_rate,
                    rx_sample_rate_hz: shared.rx_sample_rate_hz.clone(),
                    samples_per_packet,
                    fanout: Some(fanout),
                    slot_rates: Vec::new(),
                    block_samples,
                    device_rate_hz: config.device_rate_hz,
                };
                let rx_socket = shared.data_socket.try_clone().map_err(internal)?;
                let rx_thread = std::thread::Builder::new()
                    .name("radiod-rx".into())
                    .spawn(move || rx_bridge.run(rx_half, rx_socket, rx_cmd_rx))
                    .map_err(internal)?;
                (Some(rx_cmds), Some(rx_thread))
            }
            None => (None, None),
        };

        info!(
            "radiod: composite stream open at {} Hz, TX center {} Hz, RX center {} Hz, {block_samples}-sample grid block, {:.3} ms block target, {:.3} ms missing-data deadline, {:.1} dB slice headroom",
            config.device_rate_hz,
            config.tx_center_hz,
            config.rx_center_hz,
            config.block_duration_ms,
            config.tx_deadline_ms,
            config.tx_digital_backoff_db
        );
        *state = State::Running(RunningState {
            tx_thread,
            rx_thread,
            tx_cmds,
            rx_cmds,
            samples_per_packet,
            recorded,
        });
        Ok(())
    }
}

async fn await_reply<T>(rx: oneshot::Receiver<Result<T, Error>>) -> Result<T, Status> {
    rx.await
        .map_err(|_| Status::unavailable("radio bridge dropped the request"))?
        .map_err(internal)
}

#[tonic::async_trait]
impl RadioControlService for RadioControlSvc {
    async fn get_info(&self, _request: Request<()>) -> Result<Response<proto::RadioInfo>, Status> {
        if let Some(registry) = self.shared.composite.as_ref() {
            let key = client_key(&_request)?;
            let guard = registry.lock();
            let client = guard.get(key);
            // Each client sees its own rates. The clock is shared and real, so every
            // client reads the same tick rate and the same device time.
            let tx_rate = client.and_then(|c| c.tx.sample_rate_hz);
            let rx_rate = client
                .filter(|c| c.rx_configured)
                .and_then(|c| c.rx.sample_rate_hz);
            let config = *guard.config();
            drop(guard);
            let range = |(minimum, maximum): (u64, u64)| proto::ValueRange {
                minimum: minimum as f64,
                maximum: maximum as f64,
            };
            let tick_rate = match &*self.shared.state.lock() {
                State::Idle(idle) => idle.radio.tick_rate(),
                State::Running(running) => running.recorded.tick_rate,
                State::Unavailable => return Err(Status::unavailable("radio unavailable")),
            };
            let capabilities = crate::network::encode_capabilities(
                self.shared.tx_capabilities,
                self.shared.rx_supported,
            )
            .into_iter()
            // The epoch is shared, so no client may reset it.
            .filter(|&capability| capability != proto::RadioCapability::ClockReset as i32)
            .collect();
            return Ok(Response::new(proto::RadioInfo {
                actual_tx_sample_rate_hz: tx_rate,
                actual_rx_sample_rate_hz: rx_rate,
                tick_rate,
                capabilities,
                max_samples_per_packet: wire::MAX_SAMPLES_PER_PACKET as u32,
                backend: self.shared.backend.clone(),
                rx_to_tx_delay_ns: self.shared.radio_calibration.rx_to_tx_delay_ns,
                tx_full_scale_power_estimate_dbm: self
                    .shared
                    .radio_calibration
                    .tx_full_scale_power_estimate_dbm
                    .map(f64::from),
                rx_reference_dbm: current_rx_reference(&self.shared).map(f64::from),
                tx_sample_rate_range: Some(range(config.client_rate_range_hz())),
                rx_sample_rate_range: Some(range(config.client_rate_range_hz())),
                tx_frequency_range: Some(range(config.frequency_range_hz(Direction::Tx))),
                rx_frequency_range: Some(range(config.frequency_range_hz(Direction::Rx))),
                tx_gain_range: None,
                tx_gain_db: None,
            }));
        }
        let (tx_rate, rx_rate, tick_rate) = match &*self.shared.state.lock() {
            State::Idle(idle) => (
                // Report what a client negotiated, not whatever rate the driver came up
                // at, so an unconfigured stream reads as absent.
                idle.recorded.actual_tx_sample_rate_hz,
                idle.recorded.actual_rx_sample_rate_hz,
                idle.radio.tick_rate(),
            ),
            State::Running(running) => (
                running.recorded.actual_tx_sample_rate_hz,
                running.recorded.actual_rx_sample_rate_hz,
                running.recorded.tick_rate,
            ),
            State::Unavailable => return Err(Status::unavailable("radio unavailable")),
        };
        Ok(Response::new(proto::RadioInfo {
            actual_tx_sample_rate_hz: tx_rate,
            actual_rx_sample_rate_hz: rx_rate,
            tick_rate,
            capabilities: crate::network::encode_capabilities(
                self.shared.tx_capabilities,
                self.shared.rx_supported,
            ),
            max_samples_per_packet: wire::MAX_SAMPLES_PER_PACKET as u32,
            backend: self.shared.backend.clone(),
            rx_to_tx_delay_ns: self.shared.radio_calibration.rx_to_tx_delay_ns,
            tx_full_scale_power_estimate_dbm: current_tx_estimate(&self.shared).map(f64::from),
            rx_reference_dbm: current_rx_reference(&self.shared).map(f64::from),
            tx_sample_rate_range: None,
            rx_sample_rate_range: None,
            tx_frequency_range: None,
            rx_frequency_range: None,
            tx_gain_range: self
                .shared
                .tx_gain_range_db
                .map(|(minimum, maximum)| proto::ValueRange { minimum, maximum }),
            tx_gain_db: current_tx_gain(&self.shared),
        }))
    }

    async fn set_tx_frequency(
        &self,
        request: Request<proto::SetTxFrequencyRequest>,
    ) -> Result<Response<()>, Status> {
        let key = client_key(&request);
        let frequency_hz = request.into_inner().frequency_hz;
        if let Some(registry) = self.composite() {
            // The device LO is the operator's, so this names the client's own carrier
            // inside the shared span.
            let key = key?;
            let plan = {
                let mut guard = registry.lock();
                guard.set_tx_carrier(key, frequency_hz).map_err(internal)?;
                // A client that has already split retunes its live slice.
                match guard.get(key).and_then(|client| client.tx_slot) {
                    Some(slot) => {
                        let gain = guard.config().tx_scale();
                        let (plan, _) = guard.plan(key).map_err(internal)?;
                        plan.map(|plan| (slot, plan, gain))
                    }
                    None => None,
                }
            };
            if let Some((slot, plan, gain)) = plan {
                let (reply, reply_rx) = oneshot::channel();
                self.shared.tx_command(TxCommand::RetuneSlot {
                    slot,
                    plan,
                    gain,
                    reply,
                })?;
                await_reply(reply_rx).await?;
            }
            return Ok(Response::new(()));
        }
        let frequency = usize::try_from(frequency_hz)
            .map_err(|_| Status::invalid_argument("frequency exceeds platform usize"))?;
        let running = matches!(&*self.shared.state.lock(), State::Running(_));
        if running {
            let (reply, reply_rx) = oneshot::channel();
            self.shared.tx_command(TxCommand::SetFrequencyHz {
                frequency_hz: frequency_hz as f64,
                reply,
            })?;
            await_reply(reply_rx).await?;
            return Ok(Response::new(()));
        }
        self.shared.apply_setting(
            "TX frequency",
            frequency_hz,
            |r| &mut r.tx_frequency_hz,
            |radio| radio.set_tx_frequency(frequency),
        )?;
        Ok(Response::new(()))
    }

    async fn set_tx_sample_rate(
        &self,
        request: Request<proto::SetTxSampleRateRequest>,
    ) -> Result<Response<proto::SetTxSampleRateResponse>, Status> {
        let key = client_key(&request);
        let sample_rate_hz = request.into_inner().sample_rate_hz;
        if let Some(registry) = self.composite() {
            // A composite client keeps its own rate, so the applied rate it gets back is
            // the rate it asked for and it never learns the shared device rate.
            let applied = registry
                .lock()
                .set_tx_sample_rate(key?, sample_rate_hz)
                .map_err(internal)?;
            return Ok(Response::new(proto::SetTxSampleRateResponse {
                actual_sample_rate_hz: applied,
            }));
        }
        let mut state = self.shared.state.lock();
        let actual_sample_rate_hz = match &mut *state {
            State::Idle(idle) => {
                idle.radio
                    .set_tx_sample_rate(sample_rate_hz)
                    .map_err(internal)?;
                let actual = idle.radio.tx_sample_rate_hz();
                crate::stream::positive_hz(actual, "applied TX sample rate").map_err(internal)?;
                idle.recorded.tx_sample_rate_hz = Some(sample_rate_hz);
                idle.recorded.actual_tx_sample_rate_hz = Some(actual);
                if let Some(rx_rate) = idle.radio.rx_sample_rate_hz() {
                    crate::stream::positive_hz(rx_rate, "applied RX rate").map_err(internal)?;
                    idle.recorded.actual_rx_sample_rate_hz = Some(rx_rate);
                    self.shared
                        .rx_sample_rate_hz
                        .store(rx_rate.to_bits(), Ordering::Relaxed);
                }
                actual
            }
            State::Running(running)
                if running.recorded.tx_sample_rate_hz == Some(sample_rate_hz) =>
            {
                running
                    .recorded
                    .actual_tx_sample_rate_hz
                    .ok_or_else(|| Status::internal("actual TX sample rate was not recorded"))?
            }
            State::Running(_) => {
                return Err(Status::failed_precondition(
                    "cannot change TX sample rate while a session is active",
                ));
            }
            State::Unavailable => return Err(Status::unavailable("radio unavailable")),
        };
        Ok(Response::new(proto::SetTxSampleRateResponse {
            actual_sample_rate_hz,
        }))
    }

    async fn set_tx_bandwidth(
        &self,
        request: Request<proto::SetTxBandwidthRequest>,
    ) -> Result<Response<()>, Status> {
        let key = client_key(&request);
        let bandwidth_hz = request.into_inner().bandwidth_hz;
        if let Some(registry) = self.composite() {
            // The analog filter stays the operator's, but the stated bandwidth is a real
            // statement of occupied width, so it sizes this client's slice filter.
            registry.lock().set_tx_bandwidth(key?, bandwidth_hz);
            ignored_device_setting(
                "the analog TX filter",
                format!("{bandwidth_hz} Hz, which now sizes this client's slice filter"),
            );
            return Ok(Response::new(()));
        }
        let bandwidth = usize::try_from(bandwidth_hz)
            .map_err(|_| Status::invalid_argument("bandwidth exceeds platform usize"))?;
        self.shared.apply_setting(
            "TX bandwidth",
            bandwidth_hz,
            |r| &mut r.tx_bandwidth_hz,
            |radio| radio.set_tx_bandwidth(bandwidth),
        )?;
        Ok(Response::new(()))
    }

    async fn set_tick_rate(
        &self,
        request: Request<proto::SetTickRateRequest>,
    ) -> Result<Response<proto::SetTickRateResponse>, Status> {
        let requested = request.into_inner().tick_rate;
        if self.composite().is_some() {
            return Err(Status::failed_precondition(
                "composite mode takes the device clock from the radio configuration, so a client cannot change it",
            ));
        }
        let mut state = self.shared.state.lock();
        let actual_tick_rate = match &mut *state {
            State::Idle(idle) => {
                let actual = idle.radio.set_tick_rate(requested).map_err(internal)?;
                // Achievable sample rates follow the device clock, so a change invalidates
                // whatever was negotiated against the old one.
                idle.recorded.tx_sample_rate_hz = None;
                idle.recorded.actual_tx_sample_rate_hz = None;
                idle.recorded.rx = None;
                idle.recorded.actual_rx_sample_rate_hz = None;
                actual
            }
            State::Running(running) => {
                if running.recorded.tick_rate != requested {
                    return Err(Status::failed_precondition(
                        "cannot change the device clock rate while a session is active",
                    ));
                }
                requested
            }
            State::Unavailable => return Err(Status::unavailable("radio unavailable")),
        };
        Ok(Response::new(proto::SetTickRateResponse {
            actual_tick_rate,
        }))
    }

    async fn set_tx_lo_offset(
        &self,
        request: Request<proto::SetTxLoOffsetRequest>,
    ) -> Result<Response<()>, Status> {
        if self.composite().is_some() {
            ignored_device_setting(
                "the TX LO displacement",
                format!("{} Hz", request.get_ref().offset_hz),
            );
            return Ok(Response::new(()));
        }
        let offset_hz = request.into_inner().offset_hz;
        self.shared.apply_setting(
            "TX LO offset",
            offset_hz,
            |r| &mut r.tx_lo_offset_hz,
            |radio| radio.set_tx_lo_offset_hz(offset_hz),
        )?;
        Ok(Response::new(()))
    }

    async fn setup_rx(
        &self,
        request: Request<proto::SetupRxRequest>,
    ) -> Result<Response<proto::SetupRxResponse>, Status> {
        if !self.shared.rx_supported {
            return Err(Status::failed_precondition("radio has no RX chain"));
        }
        let key = client_key(&request);
        let mut req = request.into_inner();
        if let Some(registry) = self.composite() {
            let rate = req.sample_rate_hz;
            let bandwidth = (req.bandwidth_hz > 0.0)
                .then(|| whole_hz(req.bandwidth_hz, "RX bandwidth"))
                .transpose()?;
            let applied = registry
                .lock()
                .setup_rx(key?, rate, bandwidth)
                .map_err(internal)?;
            if req.gain_db.is_some() {
                ignored_device_setting("RX gain", format!("{:?} dB", req.gain_db));
            }
            return Ok(Response::new(proto::SetupRxResponse {
                actual_sample_rate_hz: applied,
            }));
        }
        if req.antenna.is_empty() {
            req.antenna = self.shared.rx_defaults.antenna.clone();
        }
        if req.gain_db.is_none() {
            req.gain_db = self.shared.rx_defaults.gain_db;
        }

        let gain_db = {
            let mut state = self.shared.state.lock();
            match &mut *state {
                State::Idle(idle) => {
                    let actual_sample_rate_hz = idle
                        .radio
                        .setup_rx(
                            req.channel as usize,
                            &req.antenna,
                            req.sample_rate_hz,
                            req.bandwidth_hz,
                            req.gain_db,
                        )
                        .map_err(internal)?;
                    crate::stream::positive_hz(actual_sample_rate_hz, "applied RX rate")
                        .map_err(internal)?;
                    if idle.recorded.actual_tx_sample_rate_hz.is_some() {
                        idle.recorded.actual_tx_sample_rate_hz =
                            Some(idle.radio.tx_sample_rate_hz());
                    }
                    self.shared.rx_reference_bits.store(
                        idle.radio.rx_reference_dbm().unwrap_or(f32::NAN).to_bits(),
                        Ordering::Relaxed,
                    );
                    self.shared
                        .rx_sample_rate_hz
                        .store(actual_sample_rate_hz.to_bits(), Ordering::Relaxed);
                    info!(
                        "radiod: rx prepared antenna='{}' rate={} bw={} gain={:?}",
                        req.antenna, req.sample_rate_hz, req.bandwidth_hz, req.gain_db
                    );
                    idle.recorded.rx = Some(req);
                    idle.recorded.actual_rx_sample_rate_hz = Some(actual_sample_rate_hz);
                    return Ok(Response::new(proto::SetupRxResponse {
                        actual_sample_rate_hz,
                    }));
                }
                State::Running(running) => match running.recorded.rx.as_ref() {
                    Some(current) if *current == req => {
                        return Ok(Response::new(proto::SetupRxResponse {
                            actual_sample_rate_hz: running
                                .recorded
                                .actual_rx_sample_rate_hz
                                .ok_or_else(|| Status::internal("RX rate unavailable"))?,
                        }));
                    }
                    Some(current)
                        if current.channel == req.channel
                            && current.antenna == req.antenna
                            && current.sample_rate_hz == req.sample_rate_hz
                            && current.bandwidth_hz == req.bandwidth_hz =>
                    {
                        req.gain_db
                    }
                    _ => {
                        return Err(Status::failed_precondition(
                            "cannot change the RX channel, antenna, rate or bandwidth while a session is active",
                        ));
                    }
                },
                State::Unavailable => return Err(Status::unavailable("radio unavailable")),
            }
        };

        if let Some(gain_db) = gain_db {
            let (reply, reply_rx) = oneshot::channel();
            self.shared
                .rx_command(RxCommand::SetGain { gain_db, reply })?;
            let reference = await_reply(reply_rx).await?;
            self.shared
                .rx_reference_bits
                .store(reference.unwrap_or(f32::NAN).to_bits(), Ordering::Relaxed);
        }
        info!("radiod: rx prepared for a new client gain={gain_db:?}");
        if let State::Running(running) = &mut *self.shared.state.lock() {
            running.recorded.rx = Some(req);
        }
        Ok(Response::new(proto::SetupRxResponse {
            actual_sample_rate_hz: f64::from_bits(
                self.shared.rx_sample_rate_hz.load(Ordering::Relaxed),
            ),
        }))
    }

    async fn open_session(
        &self,
        request: Request<proto::OpenSessionRequest>,
    ) -> Result<Response<proto::OpenSessionResponse>, Status> {
        let composite_key = client_key(&request).ok();
        let requested = request.into_inner().samples_per_packet as usize;
        let shared = &self.shared;
        if self.composite().is_some() {
            let key = composite_key.ok_or_else(|| {
                Status::failed_precondition(
                    "composite mode identifies a client by its control connection, and this request has no peer address",
                )
            })?;
            return self
                .composite_open_session(key, requested)
                .await
                .map(Response::new);
        }
        let mut state = shared.state.lock();
        match std::mem::replace(&mut *state, State::Unavailable) {
            State::Idle(idle) => {
                let tx_sample_rate_hz = idle.recorded.actual_tx_sample_rate_hz;
                if tx_sample_rate_hz.is_none() && idle.recorded.actual_rx_sample_rate_hz.is_none() {
                    *state = State::Idle(idle);
                    return Err(Status::failed_precondition(
                        "SetTxSampleRate or SetupRx must be called before OpenSession",
                    ));
                }
                let samples_per_packet = requested.clamp(1, wire::MAX_SAMPLES_PER_PACKET);
                let mut recorded = idle.recorded;
                let (tx_half, rx_half) = match idle.radio.split() {
                    Ok(halves) => halves,
                    Err(e) => {
                        warn!("radiod: radio split failed: {e}");
                        return Err(internal(e));
                    }
                };

                if tx_half.tick_rate() == 0
                    || rx_half.as_ref().is_some_and(|rx| rx.tick_rate() == 0)
                {
                    return Err(Status::failed_precondition(
                        "stream clock rate must be positive",
                    ));
                }
                // One tick rate describes the whole session, so the halves must agree.
                if let Some(rx) = rx_half.as_ref()
                    && rx.tick_rate() != tx_half.tick_rate()
                {
                    return Err(Status::failed_precondition(format!(
                        "TX clock {} Hz and RX clock {} Hz must match",
                        tx_half.tick_rate(),
                        rx.tick_rate()
                    )));
                }
                // Timestamps on this session are in these ticks, so GetInfo must keep
                // reporting the rate after the radio has been split away.
                recorded.tick_rate = tx_half.tick_rate();

                let session_id = new_session_id();
                let peer: crate::daemon::bridge::SharedReturnPath = Arc::new(Mutex::new(None));
                shared.sessions.replace_with_one(Session {
                    session_id,
                    peer,
                    tx_slot: tx_sample_rate_hz.map(|_| 0),
                    rx_slot: Some(0),
                });

                let (tx_cmds, tx_cmd_rx) = crossbeam_channel::unbounded();
                let tx_bridge = TxBridge {
                    sessions: shared.sessions.clone(),
                    tick_rate: recorded.tick_rate,
                    slot_rates: tx_sample_rate_hz.into_iter().collect(),
                    tcp_listener: shared.data_listener.clone(),
                    grid: None,
                    composite_deadline: std::time::Duration::ZERO,
                    device_rate_hz: 0,
                };
                let tx_socket = shared.data_socket.try_clone().map_err(internal)?;
                let tx_thread = std::thread::Builder::new()
                    .name("radiod-tx".into())
                    .spawn(move || tx_bridge.run(tx_half, tx_socket, tx_cmd_rx))
                    .map_err(internal)?;

                let rx_half = recorded.actual_rx_sample_rate_hz.and(rx_half);
                let (rx_cmds, rx_thread) = match rx_half {
                    Some(rx_half) => {
                        let (rx_cmds, rx_cmd_rx) = crossbeam_channel::unbounded();
                        let rx_bridge = RxBridge {
                            sessions: shared.sessions.clone(),
                            tick_rate: rx_half.tick_rate(),
                            rx_sample_rate_hz: shared.rx_sample_rate_hz.clone(),
                            samples_per_packet,
                            fanout: None,
                            slot_rates: Vec::new(),
                            block_samples: 0,
                            device_rate_hz: 0,
                        };
                        let rx_socket = shared.data_socket.try_clone().map_err(internal)?;
                        let rx_thread = std::thread::Builder::new()
                            .name("radiod-rx".into())
                            .spawn(move || rx_bridge.run(rx_half, rx_socket, rx_cmd_rx))
                            .map_err(internal)?;
                        (Some(rx_cmds), Some(rx_thread))
                    }
                    None => (None, None),
                };

                info!(
                    "radiod: session {session_id:#010x} open, samples_per_packet={samples_per_packet}"
                );
                *state = State::Running(RunningState {
                    tx_thread,
                    rx_thread,
                    tx_cmds,
                    rx_cmds,
                    samples_per_packet,
                    recorded,
                });
                Ok(Response::new(proto::OpenSessionResponse {
                    session_id,
                    data_port: shared.data_port as u32,
                    samples_per_packet: samples_per_packet as u32,
                }))
            }
            running @ State::Running(_) => {
                let State::Running(ref session) = running else {
                    unreachable!()
                };
                let session_id = new_session_id();
                let samples_per_packet = session.samples_per_packet;
                shared.sessions.replace_with_one(Session {
                    session_id,
                    peer: Arc::new(Mutex::new(None)),
                    tx_slot: session.recorded.actual_tx_sample_rate_hz.map(|_| 0),
                    rx_slot: Some(0),
                });
                warn!(
                    "radiod: superseding active session with {session_id:#010x} (client reconnect)"
                );
                *state = running;
                Ok(Response::new(proto::OpenSessionResponse {
                    session_id,
                    data_port: shared.data_port as u32,
                    samples_per_packet: samples_per_packet as u32,
                }))
            }
            State::Unavailable => Err(Status::unavailable("radio unavailable")),
        }
    }

    async fn get_hardware_time(
        &self,
        _request: Request<()>,
    ) -> Result<Response<proto::HardwareTime>, Status> {
        // Before a session the radio is still whole, so it answers directly.
        if let State::Idle(idle) = &*self.shared.state.lock() {
            let time_ticks = idle.radio.hardware_time().map_err(internal)?;
            return Ok(Response::new(proto::HardwareTime { time_ticks }));
        }
        let (reply, reply_rx) = oneshot::channel();
        self.shared.tx_command(TxCommand::GetHardwareTime(reply))?;
        let time_ticks = await_reply(reply_rx).await?;
        Ok(Response::new(proto::HardwareTime { time_ticks }))
    }

    async fn set_hardware_time(
        &self,
        request: Request<proto::HardwareTime>,
    ) -> Result<Response<()>, Status> {
        let time_ticks = request.into_inner().time_ticks;
        if self.composite().is_some() {
            // Ignoring this would leave the client scheduling against an epoch that does
            // not exist, and moving it would invalidate every peer's queued timestamps.
            return Err(Status::failed_precondition(
                "composite mode shares one device clock between clients, so its epoch cannot be reset",
            ));
        }
        let (reply, reply_rx) = oneshot::channel();
        self.shared
            .tx_command(TxCommand::SetHardwareTime { time_ticks, reply })?;
        await_reply(reply_rx).await?;
        Ok(Response::new(()))
    }

    async fn enable_transmit(
        &self,
        request: Request<proto::EnableTransmitRequest>,
    ) -> Result<Response<()>, Status> {
        let key = client_key(&request);
        let req = request.into_inner();
        let (reply, reply_rx) = oneshot::channel();
        let slot = key.ok().and_then(|key| self.composite_tx_slot(key));
        // Without a TX slot the bridge would act on slot zero.
        let has_tx = if self.composite().is_some() {
            slot.is_some()
        } else {
            match &*self.shared.state.lock() {
                State::Running(running) => running.recorded.actual_tx_sample_rate_hz.is_some(),
                _ => true,
            }
        };
        if !has_tx {
            if !req.enable {
                return Ok(Response::new(()));
            }
            return Err(Status::failed_precondition(
                "this session has no TX stream, so it cannot transmit",
            ));
        }
        self.shared.tx_command(TxCommand::EnableTransmit {
            enable: req.enable,
            time_ticks: req.time_ticks,
            slot,
            reply,
        })?;
        await_reply(reply_rx).await?;
        Ok(Response::new(()))
    }

    async fn rx_activate(
        &self,
        request: Request<proto::RxActivateRequest>,
    ) -> Result<Response<()>, Status> {
        let key = client_key(&request);
        let time_ticks = request.into_inner().time_ticks;
        if let Some(registry) = self.composite() {
            let key = key?;
            // The receiver runs for the whole device, so only the first client starts it.
            let (active, slot, session_id, plan) = {
                let mut guard = registry.lock();
                let active = guard.set_rx_active(key, true);
                let client = guard
                    .get(key)
                    .ok_or_else(|| Status::failed_precondition("no composite session is open"))?;
                let (slot, session_id) = (client.rx_slot, client.session_id);
                let plan = match slot {
                    Some(_) => None,
                    // A previous stop released this client's slice, so claim a new one.
                    None => guard.plan(key).map_err(internal)?.1,
                };
                (active, slot, session_id, plan)
            };
            let slot = match (slot, plan) {
                (Some(slot), _) => Some(slot),
                (None, Some(plan)) => {
                    let device_rate_hz = registry.lock().config().device_rate_hz;
                    let (reply, reply_rx) = oneshot::channel();
                    self.shared.rx_command(RxCommand::AddSlot {
                        plan: plan.clone(),
                        sample_rate_hz: device_rate_hz as f64 * plan.denominator as f64
                            / plan.ratio as f64,
                        reply,
                    })?;
                    let slot = await_reply(reply_rx).await?;
                    registry.lock().set_rx_slot(key, Some(slot));
                    Some(slot)
                }
                (None, None) => None,
            };
            // Put this client back in the fan-out, which a previous stop took it out of.
            if let Some((slot, session_id)) = slot.zip(session_id)
                && let Some(session) = self.shared.sessions.find(session_id)
                && session.rx_slot != Some(slot)
            {
                self.shared.sessions.insert(Session {
                    rx_slot: Some(slot),
                    ..session
                });
            }
            if active > 1 {
                return Ok(Response::new(()));
            }
        }
        let (reply, reply_rx) = oneshot::channel();
        self.shared
            .rx_command(RxCommand::Activate { time_ticks, reply })?;
        await_reply(reply_rx).await?;
        Ok(Response::new(()))
    }

    async fn rx_stop(&self, _request: Request<()>) -> Result<Response<()>, Status> {
        if let Some(registry) = self.composite() {
            let key = client_key(&_request)?;
            let (remaining, slot) = {
                let mut guard = registry.lock();
                let remaining = guard.set_rx_active(key, false);
                let slot = guard.get(key).and_then(|client| client.rx_slot);
                // Take this client out of the fan-out so it stops receiving, and leave
                // the device running while any peer still wants it.
                if let Some(client) = guard.get(key)
                    && let Some(session_id) = client.session_id
                    && let Some(session) = self.shared.sessions.find(session_id)
                {
                    self.shared.sessions.insert(Session {
                        rx_slot: None,
                        ..session
                    });
                }
                guard.set_rx_slot(key, None);
                (remaining, slot)
            };
            // Release the slice itself, so a client that comes and goes cannot grow the
            // fan-out and a dead one costs nothing per block.
            if let Some(slot) = slot {
                let (reply, reply_rx) = oneshot::channel();
                self.shared
                    .rx_command(RxCommand::RetireSlot { slot, reply })?;
                await_reply(reply_rx).await?;
            }
            if remaining > 0 {
                info!(
                    "radiod: composite client stopped receiving, {remaining} peer(s) keep the device receiver running"
                );
                return Ok(Response::new(()));
            }
        }
        let (reply, reply_rx) = oneshot::channel();
        self.shared.rx_command(RxCommand::Stop { reply })?;
        await_reply(reply_rx).await?;
        Ok(Response::new(()))
    }

    async fn set_rx_frequency(
        &self,
        request: Request<proto::SetRxFrequencyRequest>,
    ) -> Result<Response<()>, Status> {
        let key = client_key(&request);
        let frequency_hz = request.into_inner().frequency_hz;
        if let Some(registry) = self.composite() {
            let carrier = whole_hz(frequency_hz, "RX center frequency")?;
            let key = key?;
            let plan = {
                let mut guard = registry.lock();
                guard.set_rx_carrier(key, carrier).map_err(internal)?;
                match guard.get(key).and_then(|client| client.rx_slot) {
                    Some(slot) => {
                        let (_, plan) = guard.plan(key).map_err(internal)?;
                        plan.map(|plan| (slot, plan))
                    }
                    None => None,
                }
            };
            if let Some((slot, plan)) = plan {
                let (reply, reply_rx) = oneshot::channel();
                self.shared
                    .rx_command(RxCommand::RetuneSlot { slot, plan, reply })?;
                await_reply(reply_rx).await?;
            }
            return Ok(Response::new(()));
        }
        let (reply, reply_rx) = oneshot::channel();
        self.shared.rx_command(RxCommand::SetFrequency {
            frequency_hz,
            reply,
        })?;
        await_reply(reply_rx).await?;
        Ok(Response::new(()))
    }

    async fn set_tx_gain(
        &self,
        request: Request<proto::SetTxGainRequest>,
    ) -> Result<Response<proto::SetTxGainResponse>, Status> {
        let gain_db = request.into_inner().gain_db;
        if !gain_db.is_finite() {
            return Err(Status::invalid_argument(format!(
                "TX gain {gain_db} dB is not finite"
            )));
        }
        if self.composite().is_some() {
            // One analog gain serves every slice, so a client cannot own it.
            ignored_device_setting("TX gain", format!("{gain_db} dB"));
            return Ok(Response::new(proto::SetTxGainResponse {
                actual_gain_db: gain_db,
            }));
        }
        let running = {
            let mut state = self.shared.state.lock();
            match &mut *state {
                State::Idle(idle) => {
                    let applied = idle.radio.set_tx_gain(gain_db).map_err(internal)?;
                    Some((applied, idle.radio.tx_full_scale_power_estimate_dbm()))
                }
                State::Running(_) => None,
                State::Unavailable => return Err(Status::unavailable("radio unavailable")),
            }
        };
        let (applied, estimate) = match running {
            Some(result) => result,
            None => {
                let (reply, reply_rx) = oneshot::channel();
                self.shared
                    .tx_command(TxCommand::SetGain { gain_db, reply })?;
                await_reply(reply_rx).await?
            }
        };
        self.shared
            .tx_gain_bits
            .store(applied.to_bits(), Ordering::Relaxed);
        self.shared
            .tx_estimate_bits
            .store(estimate.unwrap_or(f32::NAN).to_bits(), Ordering::Relaxed);
        Ok(Response::new(proto::SetTxGainResponse {
            actual_gain_db: applied,
        }))
    }

    async fn set_rx_gain(
        &self,
        request: Request<proto::SetRxGainRequest>,
    ) -> Result<Response<()>, Status> {
        let gain_db = request.into_inner().gain_db;
        if self.composite().is_some() {
            // One analog gain serves every slice, so a client cannot own it.
            ignored_device_setting("RX gain", format!("{gain_db} dB"));
            return Ok(Response::new(()));
        }
        let (reply, reply_rx) = oneshot::channel();
        self.shared
            .rx_command(RxCommand::SetGain { gain_db, reply })?;
        let reference = await_reply(reply_rx).await?;
        self.shared
            .rx_reference_bits
            .store(reference.unwrap_or(f32::NAN).to_bits(), Ordering::Relaxed);
        Ok(Response::new(()))
    }
}

/// Runs a radio server, taking ownership of `radio`. Control requests use gRPC on
/// `listen`. IQ uses UDP and TCP on `data_listen`.
///
/// Set `rx_supported` to the receive capability clients should see. `backend` is
/// the driver name returned in control responses. Calibration comes from the radio.
///
/// Port zero requests ephemeral ports. `ready` receives `(control_addr, data_addr)`
/// after all listeners bind. IQ uses the same data port for TCP and UDP.
/// Complete `shutdown` to stop TX and RX, join the stream workers, and close the
/// server. Cancelling this function does not guarantee that cleanup. Listener,
/// control-server, and stream-stop errors are returned.
pub async fn serve(
    radio: Box<dyn Radio>,
    rx_supported: bool,
    backend: String,
    rx_defaults: RxDefaults,
    composite: Option<CompositeConfig>,
    listen: SocketAddr,
    data_listen: SocketAddr,
    ready: Option<oneshot::Sender<(SocketAddr, SocketAddr)>>,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<(), Error> {
    let radio_calibration = RadioCalibration {
        rx_to_tx_delay_ns: radio.rx_to_tx_delay_ns(),
        tx_full_scale_power_estimate_dbm: radio.tx_full_scale_power_estimate_dbm(),
        rx_reference_dbm: radio.rx_reference_dbm(),
    };
    let (data_socket, data_listener) = bind_data_listeners(data_listen)?;
    set_receive_buffer(&data_socket, DATA_SOCKET_RECEIVE_BYTES);
    let data_addr = data_socket.local_addr()?;
    let data_listener = Arc::new(data_listener);

    let listener = tokio::net::TcpListener::bind(listen).await?;
    let control_addr = listener.local_addr()?;

    let tx_capabilities = radio.tx_capabilities();
    // One analog gain serves every composite slice, so only a single client may set it.
    let tx_gain_range_db = composite
        .is_none()
        .then(|| radio.tx_gain_range_db())
        .flatten();
    let tx_gain_db = tx_gain_range_db.and_then(|_| radio.tx_gain_db());
    let shared = Arc::new(Shared {
        state: Mutex::new(State::Idle(IdleState {
            radio,
            recorded: RecordedSettings::default(),
        })),
        sessions: Sessions::new(),
        composite: composite.map(|config| Arc::new(Mutex::new(ClientRegistry::new(config)))),
        data_socket,
        data_listener,
        data_port: data_addr.port(),
        rx_supported,
        tx_capabilities,
        backend,
        rx_defaults,
        rx_reference_bits: AtomicU32::new(
            radio_calibration
                .rx_reference_dbm
                .unwrap_or(f32::NAN)
                .to_bits(),
        ),
        tx_estimate_bits: AtomicU32::new(
            radio_calibration
                .tx_full_scale_power_estimate_dbm
                .unwrap_or(f32::NAN)
                .to_bits(),
        ),
        radio_calibration,
        rx_sample_rate_hz: Arc::new(AtomicU64::new(0)),
        tx_gain_range_db,
        tx_gain_bits: AtomicU64::new(tx_gain_db.unwrap_or(f64::NAN).to_bits()),
    });

    info!("radiod: control plane on {control_addr}, data plane on {data_addr}");
    if let Some(ready) = ready {
        let _ = ready.send((control_addr, data_addr));
    }

    // A composite client is identified by its control connection, so that connection
    // closing is what reports the client leaving, whatever its data transport does.
    let watch = shared
        .composite
        .as_ref()
        .map(|registry| ClientWatch::new(shared.sessions.clone(), registry.clone()));
    let incoming = tokio_stream::StreamExt::map(
        tokio_stream::wrappers::TcpListenerStream::new(listener),
        move |accepted| accepted.map(|stream| WatchedStream::new(stream, watch.clone())),
    );
    let control_result = tonic::transport::Server::builder()
        .add_service(RadioControlServiceServer::new(RadioControlSvc {
            shared: shared.clone(),
        }))
        .serve_with_incoming_shutdown(incoming, shutdown)
        .await;
    let stopped = shutdown_streams(&shared).await;
    stopped?;
    control_result.map_err(|e| {
        let context = format!("radiod: control plane failed: {e}");
        Error::from(e).context(context)
    })
}

const DATA_SOCKET_RECEIVE_BYTES: usize = 8 * 1024 * 1024;

fn set_receive_buffer(socket: &UdpSocket, bytes: usize) {
    let socket = socket2::SockRef::from(socket);
    if let Err(error) = socket.set_recv_buffer_size(bytes) {
        log::warn!("radiod: could not size the data socket receive buffer: {error}");
    }
    match socket.recv_buffer_size() {
        Ok(actual) => {
            let level = if actual < bytes {
                log::Level::Warn
            } else {
                log::Level::Info
            };
            log::log!(
                level,
                "radiod: data socket receive buffer {} bytes (asked for {})",
                actual,
                bytes
            );
        }
        Err(error) => log::warn!("radiod: could not read the data socket receive buffer: {error}"),
    }
}

async fn shutdown_streams(shared: &Shared) -> Result<(), Error> {
    // No client may keep sending into a stopped stream.
    shared.sessions.clear();
    let state = std::mem::replace(&mut *shared.state.lock(), State::Unavailable);
    let State::Running(running) = state else {
        return Ok(());
    };
    let (tx_reply, tx_done) = oneshot::channel();
    let tx_sent = running
        .tx_cmds
        .send(TxCommand::Shutdown { reply: tx_reply });
    let rx_done = if let Some(rx_cmds) = running.rx_cmds {
        let (reply, done) = oneshot::channel();
        rx_cmds.send(RxCommand::Shutdown { reply })?;
        Some(done)
    } else {
        None
    };
    let tx_result = match tx_sent {
        Ok(()) => tx_done.await?,
        Err(error) => Err(error.into()),
    };
    let rx_result = match rx_done {
        Some(done) => done.await?,
        None => Ok(()),
    };
    tokio::task::spawn_blocking(move || -> Result<(), Error> {
        running.tx_thread.join().map_err(|_| "TX bridge panicked")?;
        if let Some(rx_thread) = running.rx_thread {
            rx_thread.join().map_err(|_| "RX bridge panicked")?;
        }
        Ok(())
    })
    .await??;
    tx_result?;
    rx_result?;
    info!("radiod: TX and RX stopped");
    Ok(())
}

fn current_tx_estimate(shared: &Shared) -> Option<f32> {
    let value = f32::from_bits(shared.tx_estimate_bits.load(Ordering::Relaxed));
    value.is_finite().then_some(value)
}

fn current_tx_gain(shared: &Shared) -> Option<f64> {
    let value = f64::from_bits(shared.tx_gain_bits.load(Ordering::Relaxed));
    value.is_finite().then_some(value)
}

fn current_rx_reference(shared: &Shared) -> Option<f32> {
    let value = f32::from_bits(shared.rx_reference_bits.load(Ordering::Relaxed));
    value.is_finite().then_some(value)
}

fn bind_data_listeners(address: SocketAddr) -> Result<(UdpSocket, std::net::TcpListener), Error> {
    const EPHEMERAL_BIND_ATTEMPTS: usize = 16;
    for attempt in 0..EPHEMERAL_BIND_ATTEMPTS {
        let udp = UdpSocket::bind(address)?;
        match std::net::TcpListener::bind(udp.local_addr()?) {
            Ok(tcp) => return Ok((udp, tcp)),
            Err(error)
                if address.port() == 0
                    && error.kind() == std::io::ErrorKind::AddrInUse
                    && attempt + 1 < EPHEMERAL_BIND_ATTEMPTS =>
            {
                continue;
            }
            Err(error) => return Err(error.into()),
        }
    }
    unreachable!()
}
