use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::error::Error;
use crossbeam_channel::{Receiver, Sender, TrySendError};
use log::{debug, info, warn};
use num_complex::Complex32;
use tonic::transport::{Channel, Endpoint};

use super::clock::NetworkClock;
use super::proto;
use super::proto::radio_control_service_client::RadioControlServiceClient;
use super::wire::{
    self, FLAG_RX_DISCONTINUITY, FLAG_TX_BLOCK_END, FLAG_TX_BLOCK_START, FLAG_TX_DISABLE,
    FLAG_TX_ENABLE, FLAG_TX_NO_TICK, PacketHeader, StreamKind, Transport, decode_header,
    decode_samples_into, encode_packet,
};
use crate::{Radio, RadioRx, RadioTx, RxReadResult};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const RPC_TIMEOUT: Duration = Duration::from_secs(5);
const HELLO_REPEATS: usize = 3;
const RX_PACKET_QUEUE_CAPACITY: usize = 512;
const RECV_BUF_LEN: usize = 65536;
const RECEIVER_POLL: Duration = Duration::from_millis(250);
const SOCKET_BUFFER_BYTES: usize = 4 * 1024 * 1024;
const MAX_TX_BLOCK_SAMPLES: usize = 32_768;
// Pace UDP writes to avoid socket overflow.
const TX_PACING_NUM: u32 = 21;
const TX_PACING_DEN: u32 = 20;
const TX_PACING_SLACK: Duration = Duration::from_micros(500);
const TX_PACING_IDLE: Duration = Duration::from_millis(50);

/// Remote IQ packet and transport options. Defaults to 350 samples per packet, UDP, and the control host.
#[derive(Debug, Clone)]
pub struct NetworkRadioOptions {
    /// Requested packet sample count. The server negotiates and caps the value at session open.
    pub samples_per_packet: usize,
    /// IQ transport, used for TX and RX alike.
    pub transport: Transport,
    /// Optional IQ host override. `None` uses the control endpoint host. The data port comes from the server.
    pub data_host: Option<String>,
}

impl Default for NetworkRadioOptions {
    fn default() -> Self {
        Self {
            samples_per_packet: wire::DEFAULT_SAMPLES_PER_PACKET,
            transport: Transport::default(),
            data_host: None,
        }
    }
}

/// Settings a server accepts from a client, in Hz. `None` means no limit beyond the device.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AcceptedRanges {
    /// TX sample rates.
    pub tx_sample_rate_hz: Option<std::ops::RangeInclusive<f64>>,
    /// RX sample rates.
    pub rx_sample_rate_hz: Option<std::ops::RangeInclusive<f64>>,
    /// TX center frequencies.
    pub tx_frequency_hz: Option<std::ops::RangeInclusive<f64>>,
    /// RX center frequencies.
    pub rx_frequency_hz: Option<std::ops::RangeInclusive<f64>>,
}

struct RxPacket {
    seq: u32,
    time_ticks: u64,
    discontinuity: bool,
    samples: Vec<Complex32>,
}

/// A remote radio controlled through gRPC, with IQ sent over UDP or TCP.
/// Timestamps are in nanoseconds. Calibration comes from the server.
/// TX success confirms local transport submission, not native device acceptance or RF completion.
/// UDP can lose IQ. TCP preserves bytes but does not acknowledge hardware writes.
/// Set sample rates before splitting. Splitting opens a server session and starts the UDP receiver.
pub struct NetworkRadio {
    rt: Arc<RpcRuntime>,
    client: RadioControlServiceClient<Channel>,
    info: proto::RadioInfo,
    control_host: String,
    options: NetworkRadioOptions,
    tx_sample_rate_hz: f64,
    rx_sample_rate_hz: f64,
    rx_configured: bool,
}

fn rpc_err(context: &str, status: tonic::Status) -> Error {
    let message = format!("network radio: {context}: {status}");
    Error::from(status).context(message)
}

struct RpcRuntime {
    runtime: Option<tokio::runtime::Runtime>,
}

impl RpcRuntime {
    fn new() -> Result<Self, Error> {
        Ok(Self {
            runtime: Some(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .thread_name("netradio-rpc")
                    .enable_all()
                    .build()?,
            ),
        })
    }
    fn handle(&self) -> tokio::runtime::Handle {
        self.runtime
            .as_ref()
            .expect("runtime is owned until Drop")
            .handle()
            .clone()
    }
}
impl Drop for RpcRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

fn block_on<F>(rt: &RpcRuntime, fut: F) -> F::Output
where
    F: Future + Send,
    F::Output: Send,
{
    let handle = rt.handle();
    match tokio::runtime::Handle::try_current() {
        Ok(current) if current.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| handle.block_on(fut))
        }
        Ok(_) => std::thread::scope(|scope| {
            scope
                .spawn(|| handle.block_on(fut))
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        }),
        Err(_) => handle.block_on(fut),
    }
}

impl NetworkRadio {
    /// Connects to a control endpoint formatted as `host:port`, without a URI scheme.
    /// Queries backend capabilities and calibration. Returns connection or RPC errors.
    ///
    /// Control RPCs use an owned executor in every caller context, including current-thread Tokio.
    /// Calls block the caller. Use a blocking worker when that would stall application tasks.
    pub fn connect(control_addr: &str, options: NetworkRadioOptions) -> Result<Self, Error> {
        let rt = Arc::new(RpcRuntime::new()?);
        let control_host = control_addr
            .rsplit_once(':')
            .map(|(host, _)| host.to_string())
            .ok_or_else(|| Error::from(format!("network radio: bad address {control_addr}")))?;
        let endpoint = Endpoint::from_shared(format!("http://{control_addr}"))
            .map_err(|e| format!("network radio: bad address {control_addr}: {e}"))?
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(RPC_TIMEOUT);
        let channel = block_on(&rt, endpoint.connect()).map_err(|e| {
            let context = format!("network radio: connect {control_addr}: {e}");
            Error::from(e).context(context)
        })?;
        let mut client = RadioControlServiceClient::new(channel);
        let info = block_on(&rt, client.get_info(()))
            .map_err(|status| rpc_err("get_info", status))?
            .into_inner();
        info!(
            "network radio: connected to {control_addr} backend={} rx_supported={} max_samples_per_packet={}",
            info.backend,
            info.capabilities
                .contains(&(proto::RadioCapability::Receive as i32)),
            info.max_samples_per_packet
        );
        Ok(Self {
            rt,
            client,
            info,
            control_host,
            options,
            tx_sample_rate_hz: 0.0,
            rx_sample_rate_hz: 0.0,
            rx_configured: false,
        })
    }

    /// Backend name the server reported.
    pub fn backend(&self) -> &str {
        &self.info.backend
    }

    /// Whether the server can receive.
    pub fn supports_rx(&self) -> bool {
        self.info
            .capabilities
            .contains(&(proto::RadioCapability::Receive as i32))
    }

    /// Device time in ticks before the split. Needs a running server session.
    pub fn hardware_time(&self) -> Result<u64, Error> {
        let mut client = self.client.clone();
        self.block_on("get_hardware_time", client.get_hardware_time(()))
            .map(|time| time.time_ticks)
    }

    /// Settings the server accepts, as of the most recent server info.
    pub fn accepted_ranges(&self) -> AcceptedRanges {
        let range = |range: Option<proto::ValueRange>| range.map(|r| r.minimum..=r.maximum);
        AcceptedRanges {
            tx_sample_rate_hz: range(self.info.tx_sample_rate_range),
            rx_sample_rate_hz: range(self.info.rx_sample_rate_range),
            tx_frequency_hz: range(self.info.tx_frequency_range),
            rx_frequency_hz: range(self.info.rx_frequency_range),
        }
    }

    fn block_on<F, T>(&self, context: &str, fut: F) -> Result<T, Error>
    where
        F: Future<Output = Result<tonic::Response<T>, tonic::Status>> + Send,
        T: Send,
    {
        block_on(&self.rt, fut)
            .map(|resp| resp.into_inner())
            .map_err(|status| rpc_err(context, status))
    }
}

impl Radio for NetworkRadio {
    fn tx_capabilities(&self) -> crate::TxCapabilities {
        crate::network::decode_tx_capabilities(&self.info.capabilities)
    }

    fn tx_sample_rate_hz(&self) -> f64 {
        self.tx_sample_rate_hz
    }
    fn rx_sample_rate_hz(&self) -> Option<f64> {
        self.rx_configured.then_some(self.rx_sample_rate_hz)
    }
    fn supports_timed_tx(&self) -> bool {
        self.tx_capabilities()
            .supports(crate::TxCapability::TimedSamples)
    }
    fn tick_rate(&self) -> u64 {
        self.info.tick_rate
    }

    fn rx_to_tx_delay_ns(&self) -> Option<i64> {
        self.info.rx_to_tx_delay_ns
    }

    fn tx_full_scale_power_estimate_dbm(&self) -> Option<f32> {
        self.info
            .tx_full_scale_power_estimate_dbm
            .map(|dbm| dbm as f32)
    }

    fn tx_gain_range_db(&self) -> Option<(f64, f64)> {
        self.info
            .tx_gain_range
            .map(|range| (range.minimum, range.maximum))
    }

    fn tx_gain_db(&self) -> Option<f64> {
        self.info.tx_gain_db
    }

    fn set_tx_gain(&mut self, gain_db: f64) -> Result<f64, Error> {
        crate::stream::finite(gain_db, "TX gain")?;
        let mut client = self.client.clone();
        let response = self.block_on(
            "set_tx_gain",
            client.set_tx_gain(proto::SetTxGainRequest { gain_db }),
        )?;
        let mut info_client = self.client.clone();
        self.info = self.block_on("get_info", info_client.get_info(()))?;
        Ok(response.actual_gain_db)
    }

    fn rx_reference_dbm(&self) -> Option<f32> {
        self.info.rx_reference_dbm.map(|dbm| dbm as f32)
    }

    fn set_tx_frequency(&mut self, center_frequency: usize) -> Result<(), Error> {
        let mut client = self.client.clone();
        self.block_on(
            "set_tx_frequency",
            client.set_tx_frequency(proto::SetTxFrequencyRequest {
                frequency_hz: center_frequency as u64,
            }),
        )?;
        Ok(())
    }

    fn set_tick_rate(&mut self, tick_rate: u64) -> Result<u64, Error> {
        let mut client = self.client.clone();
        let response = self.block_on(
            "set_tick_rate",
            client.set_tick_rate(proto::SetTickRateRequest { tick_rate }),
        )?;
        let mut info_client = self.client.clone();
        self.info = self.block_on("get_info", info_client.get_info(()))?;
        Ok(response.actual_tick_rate)
    }

    fn set_tx_sample_rate(&mut self, sample_rate_hz: f64) -> Result<f64, Error> {
        crate::stream::positive_hz(sample_rate_hz, "set_tx_sample_rate")?;
        let mut client = self.client.clone();
        let response = self.block_on(
            "set_tx_sample_rate",
            client.set_tx_sample_rate(proto::SetTxSampleRateRequest { sample_rate_hz }),
        )?;
        crate::stream::positive_hz(response.actual_sample_rate_hz, "applied TX rate")?;
        self.tx_sample_rate_hz = response.actual_sample_rate_hz;
        // Backends that clock in samples change their tick rate with the sample rate.
        let mut info_client = self.client.clone();
        self.info = self.block_on("get_info", info_client.get_info(()))?;
        if self.rx_configured {
            let rx_rate = self.info.actual_rx_sample_rate_hz.ok_or_else(|| {
                Error::new(
                    crate::error::ErrorKind::Protocol,
                    "configured RX rate unavailable",
                )
            })?;
            crate::stream::positive_hz(rx_rate, "applied RX rate")?;
            self.rx_sample_rate_hz = rx_rate;
        }
        Ok(self.tx_sample_rate_hz)
    }

    fn set_tx_bandwidth(&mut self, bandwidth: usize) -> Result<(), Error> {
        let mut client = self.client.clone();
        self.block_on(
            "set_tx_bandwidth",
            client.set_tx_bandwidth(proto::SetTxBandwidthRequest {
                bandwidth_hz: bandwidth as u64,
            }),
        )?;
        Ok(())
    }

    fn set_tx_lo_offset_hz(&mut self, offset_hz: i64) -> Result<(), Error> {
        let mut client = self.client.clone();
        self.block_on(
            "set_tx_lo_offset",
            client.set_tx_lo_offset(proto::SetTxLoOffsetRequest { offset_hz }),
        )?;
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
        if !self
            .info
            .capabilities
            .contains(&(proto::RadioCapability::Receive as i32))
        {
            return Err("network radio: server backend has no RX".into());
        }
        let mut client = self.client.clone();
        let response = self.block_on(
            "setup_rx",
            client.setup_rx(proto::SetupRxRequest {
                channel: channel as u32,
                antenna: antenna.to_string(),
                sample_rate_hz,
                bandwidth_hz,
                gain_db,
            }),
        )?;
        crate::stream::positive_hz(response.actual_sample_rate_hz, "applied RX rate")?;
        self.rx_sample_rate_hz = response.actual_sample_rate_hz;
        self.rx_configured = true;
        let mut client = self.client.clone();
        self.info = self.block_on("get_info", client.get_info(()))?;
        if self.tx_sample_rate_hz > 0.0 {
            let tx_rate = self.info.actual_tx_sample_rate_hz.ok_or_else(|| {
                Error::new(
                    crate::error::ErrorKind::Protocol,
                    "configured TX rate unavailable",
                )
            })?;
            crate::stream::positive_hz(tx_rate, "applied TX rate")?;
            self.tx_sample_rate_hz = tx_rate;
        }
        Ok(response.actual_sample_rate_hz)
    }

    fn split(self: Box<Self>) -> Result<(Box<dyn RadioTx>, Option<Box<dyn RadioRx>>), Error> {
        if self.tx_sample_rate_hz == 0.0 && !self.rx_configured {
            return Err(
                "network radio: set_tx_sample_rate or setup_rx must run before split".into(),
            );
        }
        let mut client = self.client.clone();
        let requested = self
            .options
            .samples_per_packet
            .min(wire::MAX_SAMPLES_PER_PACKET);
        let session = self.block_on(
            "open_session",
            client.open_session(proto::OpenSessionRequest {
                samples_per_packet: requested as u32,
            }),
        )?;
        let samples_per_packet = session.samples_per_packet as usize;
        if samples_per_packet == 0 || samples_per_packet > wire::MAX_SAMPLES_PER_PACKET {
            return Err(format!(
                "network radio: server negotiated bad samples_per_packet {samples_per_packet}"
            )
            .into());
        }

        let data_host = self
            .options
            .data_host
            .clone()
            .unwrap_or_else(|| self.control_host.clone());
        let data_addr = resolve_data_addr(&data_host, session.data_port as u16)?;
        let socket = bind_matching_family(&data_addr)?;
        set_socket_buffers(&socket);
        socket.connect(data_addr)?;
        info!(
            "network radio: data plane {} -> {} session={:#010x} samples_per_packet={}",
            socket.local_addr()?,
            data_addr,
            session.session_id,
            samples_per_packet
        );

        if self.info.tick_rate == 0 {
            return Err("network radio: server reported a zero tick rate".into());
        }
        let clock = Arc::new(NetworkClock::new(
            self.info.tick_rate,
            self.sync_clock(&mut client)?,
        ));

        let shutdown = Arc::new(AtomicBool::new(false));
        let queue_gap = Arc::new(AtomicBool::new(false));
        let (rx_packet_tx, rx_packet_rx) = crossbeam_channel::bounded(RX_PACKET_QUEUE_CAPACITY);

        let hello_header = |seq: u32| PacketHeader {
            stream: StreamKind::Hello,
            flags: 0,
            session_id: session.session_id,
            seq,
            time_ticks: 0,
            sample_count: 0,
            block_id: 0,
            block_offset: 0,
            block_sample_count: 0,
        };
        let mut hello = Vec::new();

        let tx_stream = match self.options.transport {
            Transport::Udp => {
                // A lost hello leaves the server with no return address.
                for seq in 0..HELLO_REPEATS as u32 {
                    encode_packet(&hello_header(seq), &[], &mut hello)?;
                    socket.send(&hello)?;
                }
                spawn_receiver(
                    socket.try_clone()?,
                    session.session_id,
                    clock.clone(),
                    rx_packet_tx,
                    queue_gap.clone(),
                    shutdown.clone(),
                )?;
                None
            }
            Transport::Tcp => {
                let mut stream = std::net::TcpStream::connect(data_addr).map_err(|e| {
                    Error::from(format!(
                        "network radio: data stream connect {data_addr}: {e}"
                    ))
                })?;
                stream.set_nodelay(true)?;
                encode_packet(&hello_header(0), &[], &mut hello)?;
                std::io::Write::write_all(&mut stream, &hello)?;
                info!("network radio: data plane over TCP to {data_addr}");
                spawn_tcp_receiver(
                    stream.try_clone()?,
                    session.session_id,
                    clock.clone(),
                    rx_packet_tx,
                    queue_gap.clone(),
                    shutdown.clone(),
                )?;
                Some(stream)
            }
        };

        let tx = NetworkRadioTx {
            rt: self.rt.clone(),
            client: self.client.clone(),
            clock: clock.clone(),
            socket,
            session_id: session.session_id,
            seq: 0,
            block_id: 0,
            samples_per_packet,
            tx_sample_rate_hz: self.tx_sample_rate_hz,

            capabilities: self.tx_capabilities(),
            active: false,
            calibration: crate::RadioCalibration {
                rx_to_tx_delay_ns: self.rx_to_tx_delay_ns(),
                tx_full_scale_power_estimate_dbm: self.tx_full_scale_power_estimate_dbm(),
                rx_reference_dbm: self.rx_reference_dbm(),
            },
            scratch: Vec::new(),
            pace_next: Instant::now(),
            tx_stream,
            shutdown,
        };
        let rx = self.rx_configured.then(|| {
            Box::new(NetworkRadioRx {
                rt: self.rt.clone(),
                client: self.client.clone(),
                clock,
                packets: rx_packet_rx,
                pending: None,
                expected_seq: None,
                minimum_time_ticks: None,
                pending_discontinuity: false,
                queue_gap,
                rx_sample_rate_hz: self.rx_sample_rate_hz,
                rx_reference_dbm: self.rx_reference_dbm(),
            }) as Box<dyn RadioRx>
        });
        Ok((Box::new(tx), rx))
    }
}

impl NetworkRadio {
    /// Seeds the clock anchor from one device-time read. The reading is already a
    /// network transit old, so the anchor starts behind the device and heartbeats
    /// correct it. Scheduling ahead absorbs the difference.
    fn sync_clock(&self, client: &mut RadioControlServiceClient<Channel>) -> Result<u64, Error> {
        let started = Instant::now();
        let time = self.block_on("get_hardware_time", client.get_hardware_time(()))?;
        debug!(
            "network radio: clock seeded rtt={}us server_ticks={}",
            started.elapsed().as_micros(),
            time.time_ticks
        );
        Ok(time.time_ticks)
    }
}

fn resolve_data_addr(host: &str, port: u16) -> Result<SocketAddr, Error> {
    let trimmed = host.trim_start_matches('[').trim_end_matches(']');
    (trimmed, port)
        .to_socket_addrs()
        .map_err(|e| {
            let context = format!("network radio: resolve {host}:{port}: {e}");
            Error::from(e).context(context)
        })?
        .next()
        .ok_or_else(|| format!("network radio: no address for {host}:{port}").into())
}

fn bind_matching_family(peer: &SocketAddr) -> Result<UdpSocket, Error> {
    let bind_addr = if peer.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    Ok(UdpSocket::bind(bind_addr)?)
}

fn set_socket_buffers(socket: &UdpSocket) {
    let socket = socket2::SockRef::from(socket);
    for result in [
        socket.set_recv_buffer_size(SOCKET_BUFFER_BYTES),
        socket.set_send_buffer_size(SOCKET_BUFFER_BYTES),
    ] {
        if let Err(error) = result {
            debug!("network radio: socket buffer setting failed: {error}");
        }
    }
}

/// Returns false once the RX consumer is gone and the receiver should stop.
fn dispatch_packet(
    buf: &[u8],
    session_id: u32,
    clock: &NetworkClock,
    rx_packet_tx: &Sender<RxPacket>,
    queue_gap: &AtomicBool,
    dropped: &mut u64,
) -> bool {
    let header = match decode_header(buf) {
        Ok(header) => header,
        Err(e) => {
            debug!("network radio: dropping undecodable packet: {e}");
            return true;
        }
    };
    if header.session_id != session_id {
        return true;
    }
    match header.stream {
        StreamKind::ClockHeartbeat => clock.observe_server_time(header.time_ticks),
        StreamKind::RxSamples => {
            let mut samples = Vec::new();
            if decode_samples_into(buf, &header, &mut samples).is_err() {
                return true;
            }
            let packet = RxPacket {
                seq: header.seq,
                time_ticks: header.time_ticks,
                discontinuity: header.flags & FLAG_RX_DISCONTINUITY != 0,
                samples,
            };
            match rx_packet_tx.try_send(packet) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    queue_gap.store(true, Ordering::Relaxed);
                    *dropped += 1;
                    if dropped.is_power_of_two() {
                        warn!("network radio: RX queue full, dropped {dropped} packets total");
                    }
                }
                Err(TrySendError::Disconnected(_)) => return false,
            }
        }
        StreamKind::Hello | StreamKind::TxSamples => {}
    }
    true
}

fn spawn_receiver(
    socket: UdpSocket,
    session_id: u32,
    clock: Arc<NetworkClock>,
    rx_packet_tx: Sender<RxPacket>,
    queue_gap: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) -> Result<(), Error> {
    socket.set_read_timeout(Some(RECEIVER_POLL))?;
    std::thread::Builder::new()
        .name("netradio-recv".into())
        .spawn(move || {
            let mut buf = vec![0u8; RECV_BUF_LEN];
            let mut dropped: u64 = 0;
            loop {
                if shutdown.load(Ordering::Relaxed) {
                    return;
                }
                let len = match socket.recv(&mut buf) {
                    Ok(len) => len,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut
                            || e.kind() == std::io::ErrorKind::Interrupted =>
                    {
                        continue;
                    }
                    Err(e) => {
                        warn!("network radio: data socket recv failed: {e}");
                        return;
                    }
                };
                if !dispatch_packet(
                    &buf[..len],
                    session_id,
                    &clock,
                    &rx_packet_tx,
                    &queue_gap,
                    &mut dropped,
                ) {
                    return;
                }
            }
        })?;
    Ok(())
}

fn read_fully(stream: &mut std::net::TcpStream, buf: &mut [u8], stop: &AtomicBool) -> bool {
    use std::io::Read;
    let mut filled = 0;
    while filled < buf.len() {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        match stream.read(&mut buf[filled..]) {
            Ok(0) => return false,
            Ok(read) => filled += read,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                warn!("network radio: data stream read failed: {e}");
                return false;
            }
        }
    }
    true
}

fn spawn_tcp_receiver(
    mut stream: std::net::TcpStream,
    session_id: u32,
    clock: Arc<NetworkClock>,
    rx_packet_tx: Sender<RxPacket>,
    queue_gap: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) -> Result<(), Error> {
    stream.set_read_timeout(Some(RECEIVER_POLL))?;
    std::thread::Builder::new()
        .name("netradio-recv-tcp".into())
        .spawn(move || {
            let mut buf = Vec::new();
            let mut dropped: u64 = 0;
            loop {
                if shutdown.load(Ordering::Relaxed) {
                    return;
                }
                buf.clear();
                buf.resize(wire::HEADER_LEN, 0);
                if !read_fully(&mut stream, &mut buf, &shutdown) {
                    return;
                }
                let payload = match wire::payload_len(&buf) {
                    Ok(payload) => payload,
                    Err(e) => {
                        warn!("network radio: data-plane TCP framing lost: {e}");
                        return;
                    }
                };
                buf.resize(wire::HEADER_LEN + payload, 0);
                if !read_fully(&mut stream, &mut buf[wire::HEADER_LEN..], &shutdown) {
                    return;
                }
                if !dispatch_packet(
                    &buf,
                    session_id,
                    &clock,
                    &rx_packet_tx,
                    &queue_gap,
                    &mut dropped,
                ) {
                    return;
                }
            }
        })?;
    Ok(())
}

/// A transmit handle for a remote radio. Disable TX before dropping it.
/// Disable is sent after queued IQ, but does not wait for confirmation that the
/// hardware has stopped. The server also stops TX after its silence timeout.
///
/// IQ is quantized by [`wire::encode_packet`]. Writes larger than 32,768 samples
/// are split into blocks with adjusted timestamps.
struct NetworkRadioTx {
    rt: Arc<RpcRuntime>,
    client: RadioControlServiceClient<Channel>,
    clock: Arc<NetworkClock>,
    socket: UdpSocket,
    session_id: u32,
    seq: u32,
    block_id: u32,
    samples_per_packet: usize,
    tx_sample_rate_hz: f64,
    capabilities: crate::TxCapabilities,
    active: bool,
    calibration: crate::RadioCalibration,
    scratch: Vec<u8>,
    pace_next: Instant,
    tx_stream: Option<std::net::TcpStream>,
    shutdown: Arc<AtomicBool>,
}

impl NetworkRadioTx {
    fn send_samples(&mut self, samples: &[Complex32], tick: Option<u64>) -> Result<(), Error> {
        self.send_samples_with_end(samples, tick, false)
    }
    fn send_samples_with_end(
        &mut self,
        samples: &[Complex32],
        tick: Option<u64>,
        end_of_burst: bool,
    ) -> Result<(), Error> {
        if self.tx_sample_rate_hz <= 0.0 {
            return Err(Error::unsupported("TX on a receive-only network session"));
        }
        if samples.len() <= MAX_TX_BLOCK_SAMPLES {
            return self.send_block(samples, tick, end_of_burst);
        }
        let mut sent = 0usize;
        for block in samples.chunks(MAX_TX_BLOCK_SAMPLES) {
            let offset_ticks = crate::network::wire::samples_to_ticks(
                sent as u64,
                self.clock.tick_rate(),
                self.tx_sample_rate_hz,
            );
            let block_tick = tick.map(|tick| tick.saturating_add(offset_ticks));
            self.send_block(
                block,
                block_tick,
                end_of_burst && sent + block.len() == samples.len(),
            )?;
            sent += block.len();
        }
        Ok(())
    }

    fn pace(&mut self, samples: usize) {
        let now = Instant::now();
        if self.pace_next + TX_PACING_IDLE < now {
            self.pace_next = now;
        }
        if let Some(ahead) = self.pace_next.checked_duration_since(now)
            && ahead > TX_PACING_SLACK
        {
            std::thread::sleep(ahead);
        }
        let airtime = Duration::from_nanos(crate::network::wire::samples_to_ns_exact(
            samples as u64,
            self.tx_sample_rate_hz,
        ));
        self.pace_next += airtime * TX_PACING_DEN / TX_PACING_NUM;
    }

    fn send_block(
        &mut self,
        samples: &[Complex32],
        tick: Option<u64>,
        end_of_burst: bool,
    ) -> Result<(), Error> {
        if samples.is_empty() {
            return Ok(());
        }
        let block_sample_count = u32::try_from(samples.len())
            .map_err(|_| Error::from("network radio: TX block exceeds u32 sample count"))?;
        let block_id = self.block_id;
        self.block_id = self.block_id.wrapping_add(1);
        let mut offset = 0usize;
        while offset < samples.len() {
            let chunk = &samples[offset..(offset + self.samples_per_packet).min(samples.len())];
            let (mut flags, time_ticks) = match tick {
                Some(tick) => (
                    0,
                    tick.saturating_add(crate::network::wire::samples_to_ticks(
                        offset as u64,
                        self.clock.tick_rate(),
                        self.tx_sample_rate_hz,
                    )),
                ),
                None => (FLAG_TX_NO_TICK, 0),
            };
            if offset == 0 {
                flags |= FLAG_TX_BLOCK_START;
            }
            if offset + chunk.len() == samples.len() {
                flags |= FLAG_TX_BLOCK_END;
                if end_of_burst {
                    flags |= crate::network::wire::FLAG_TX_BURST_END;
                }
            }
            encode_packet(
                &PacketHeader {
                    stream: StreamKind::TxSamples,
                    flags,
                    session_id: self.session_id,
                    seq: self.seq,
                    time_ticks,
                    sample_count: chunk.len() as u16,
                    block_id,
                    block_offset: offset as u32,
                    block_sample_count,
                },
                chunk,
                &mut self.scratch,
            )?;
            match self.tx_stream.as_mut() {
                Some(stream) => {
                    use std::io::Write;
                    stream.write_all(&self.scratch)?;
                }
                None => {
                    self.pace(chunk.len());
                    self.socket.send(&self.scratch)?;
                }
            }
            self.seq = self.seq.wrapping_add(1);
            offset += chunk.len();
        }
        Ok(())
    }

    fn send_disable_fence(&mut self) -> Result<(), Error> {
        self.send_control_fence(FLAG_TX_DISABLE, None)
    }

    fn send_control_fence(&mut self, flags: u16, tick: Option<u64>) -> Result<(), Error> {
        let Some(stream) = self.tx_stream.as_mut() else {
            return Err("network radio: TX control fence requires TCP".into());
        };
        encode_packet(
            &PacketHeader {
                stream: StreamKind::TxSamples,
                flags: flags | if tick.is_none() { FLAG_TX_NO_TICK } else { 0 },
                session_id: self.session_id,
                seq: self.seq,
                time_ticks: tick.unwrap_or(0),
                sample_count: 0,
                block_id: 0,
                block_offset: 0,
                block_sample_count: 0,
            },
            &[],
            &mut self.scratch,
        )?;
        use std::io::Write;
        stream.write_all(&self.scratch)?;
        self.seq = self.seq.wrapping_add(1);
        Ok(())
    }

    fn rpc<F, T>(&self, context: &str, fut: F) -> Result<T, Error>
    where
        F: Future<Output = Result<tonic::Response<T>, tonic::Status>> + Send,
        T: Send,
    {
        block_on(&self.rt, fut)
            .map(|resp| resp.into_inner())
            .map_err(|status| rpc_err(context, status))
    }
}

impl Drop for NetworkRadioTx {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

impl RadioTx for NetworkRadioTx {
    fn capabilities(&self) -> crate::TxCapabilities {
        self.capabilities
    }
    fn write(
        &mut self,
        samples: &[Complex32],
        metadata: crate::TxMetadata,
    ) -> Result<usize, Error> {
        if metadata.schedule.ticks().is_some()
            && !self
                .capabilities
                .supports(crate::TxCapability::TimedSamples)
        {
            return Err(Error::unsupported("timed TX"));
        }
        if metadata.end_of_burst && !self.capabilities.supports(crate::TxCapability::BurstEnd) {
            return Err(Error::unsupported("burst end"));
        }
        self.send_samples_with_end(samples, metadata.schedule.ticks(), metadata.end_of_burst)?;
        Ok(samples.len())
    }

    fn end_burst(&mut self) -> Result<(), Error> {
        // Version 3 requires a nonempty IQ block, so terminate with one silent sample.
        self.write(
            &[Complex32::default()],
            crate::TxMetadata {
                schedule: crate::Schedule::Immediate,
                end_of_burst: true,
            },
        )?;
        Ok(())
    }

    fn tx_sample_rate_hz(&self) -> f64 {
        self.tx_sample_rate_hz
    }
    fn rx_to_tx_delay_ns(&self) -> Option<i64> {
        self.calibration.rx_to_tx_delay_ns
    }
    fn tx_full_scale_power_estimate_dbm(&self) -> Option<f32> {
        self.calibration.tx_full_scale_power_estimate_dbm
    }

    fn set_tx_gain(&mut self, gain_db: f64) -> Result<f64, Error> {
        crate::stream::finite(gain_db, "TX gain")?;
        let mut client = self.client.clone();
        let response = self.rpc(
            "set_tx_gain",
            client.set_tx_gain(proto::SetTxGainRequest { gain_db }),
        )?;
        let mut info_client = self.client.clone();
        let info = self.rpc("get_info", info_client.get_info(()))?;
        self.calibration.tx_full_scale_power_estimate_dbm =
            info.tx_full_scale_power_estimate_dbm.map(|dbm| dbm as f32);
        Ok(response.actual_gain_db)
    }

    fn supports_timed_tx(&self) -> bool {
        self.capabilities
            .supports(crate::TxCapability::TimedSamples)
    }
    fn tick_rate(&self) -> u64 {
        self.clock.tick_rate()
    }

    fn get_hardware_time(&self) -> Result<u64, Error> {
        Ok(self.clock.now_ticks())
    }

    fn set_hardware_time(&self, ticks: u64) -> Result<(), Error> {
        if !self.capabilities.supports(crate::TxCapability::ClockReset) {
            return Err(Error::unsupported("clock reset"));
        }
        let mut client = self.client.clone();
        self.rpc(
            "set_hardware_time",
            client.set_hardware_time(proto::HardwareTime { time_ticks: ticks }),
        )?;
        self.clock.reanchor(ticks);
        Ok(())
    }

    fn transmit(&mut self, samples: &[Complex32]) -> Result<(), Error> {
        self.send_samples(samples, None)
    }

    fn transmit_at(&mut self, samples: &[Complex32], tick: Option<u64>) -> Result<(), Error> {
        if tick.is_some() && !self.supports_timed_tx() {
            return Err(Error::unsupported("remote timed TX"));
        }
        self.send_samples(samples, tick)
    }

    fn enable_transmit(&mut self, enable: bool) -> Result<(), Error> {
        self.enable_transmit_at(enable, None)
    }

    fn enable_transmit_at(&mut self, enable: bool, tick: Option<u64>) -> Result<(), Error> {
        if tick.is_some()
            && !self
                .capabilities
                .supports(crate::TxCapability::TimedActivation)
        {
            return Err(Error::unsupported("timed activation"));
        }
        if enable && self.active && tick.is_none() {
            return Ok(());
        }
        if !enable && self.tx_stream.is_some() {
            self.send_disable_fence()?;
            self.active = false;
            return Ok(());
        }
        let mut client = self.client.clone();
        self.rpc(
            "enable_transmit",
            client.enable_transmit(proto::EnableTransmitRequest {
                enable,
                time_ticks: tick,
            }),
        )?;
        if enable && self.tx_stream.is_some() {
            self.send_control_fence(FLAG_TX_ENABLE, tick)?;
        }
        self.active = enable;
        Ok(())
    }

    fn set_tx_frequency_hz(&mut self, frequency_hz: f64) -> Result<(), Error> {
        crate::stream::positive_hz(frequency_hz, "TX frequency")?;
        if frequency_hz >= u64::MAX as f64 || frequency_hz.fract() != 0.0 {
            return Err(Error::invalid_setting(
                "remote TX frequency requires whole Hz within u64",
            ));
        }
        let mut client = self.client.clone();
        self.rpc(
            "set_tx_frequency",
            client.set_tx_frequency(proto::SetTxFrequencyRequest {
                frequency_hz: frequency_hz as u64,
            }),
        )?;
        Ok(())
    }
}

/// Remote RX half returned by [`Radio::split`] after RX setup. Reports packet loss and
/// server discontinuities through [`RxReadResult::overflow`].
struct NetworkRadioRx {
    rt: Arc<RpcRuntime>,
    client: RadioControlServiceClient<Channel>,
    clock: Arc<NetworkClock>,
    packets: Receiver<RxPacket>,
    pending: Option<(RxPacket, usize)>,
    expected_seq: Option<u32>,
    minimum_time_ticks: Option<u64>,
    pending_discontinuity: bool,
    queue_gap: Arc<AtomicBool>,
    rx_sample_rate_hz: f64,
    rx_reference_dbm: Option<f32>,
}

impl NetworkRadioRx {
    fn rpc<F, T>(&self, context: &str, fut: F) -> Result<T, Error>
    where
        F: Future<Output = Result<tonic::Response<T>, tonic::Status>> + Send,
        T: Send,
    {
        block_on(&self.rt, fut)
            .map(|resp| resp.into_inner())
            .map_err(|status| rpc_err(context, status))
    }

    fn next_packet(&mut self, wait: Option<Duration>) -> Option<(RxPacket, usize)> {
        let deadline = wait.map(|timeout| Instant::now() + timeout);
        loop {
            let (packet, consumed) = if let Some(pending) = self.pending.take() {
                pending
            } else {
                let packet = match deadline {
                    Some(deadline) => self
                        .packets
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                        .ok()?,
                    None => self.packets.recv().ok()?,
                };
                (packet, 0)
            };
            if let Some(minimum_time_ticks) = self.minimum_time_ticks {
                let tick_rate = self.clock.tick_rate();
                let end_ticks =
                    packet
                        .time_ticks
                        .saturating_add(crate::network::wire::samples_to_ticks(
                            packet.samples.len() as u64,
                            tick_rate,
                            self.rx_sample_rate_hz,
                        ));
                if end_ticks <= minimum_time_ticks {
                    continue;
                }
                let skip = if packet.time_ticks < minimum_time_ticks {
                    (((minimum_time_ticks - packet.time_ticks) as f64 * self.rx_sample_rate_hz
                        / tick_rate as f64)
                        .ceil() as usize)
                        .min(packet.samples.len())
                } else {
                    0
                };
                self.minimum_time_ticks = None;
                return Some((packet, consumed.max(skip)));
            }
            return Some((packet, consumed));
        }
    }
}

impl RadioRx for NetworkRadioRx {
    fn rx_sample_rate_hz(&self) -> f64 {
        self.rx_sample_rate_hz
    }
    fn rx_reference_dbm(&self) -> Option<f32> {
        self.rx_reference_dbm
    }

    fn tick_rate(&self) -> u64 {
        self.clock.tick_rate()
    }

    fn get_hardware_time(&self) -> Result<u64, Error> {
        Ok(self.clock.now_ticks())
    }

    fn rx_read(&mut self, buf: &mut [Complex32], timeout_us: i64) -> Result<RxReadResult, Error> {
        let deadline = if timeout_us < 0 {
            None
        } else {
            Some(
                Instant::now()
                    .checked_add(Duration::from_micros(timeout_us as u64))
                    .ok_or_else(|| {
                        Error::invalid_setting("receive timeout exceeds the host clock")
                    })?,
            )
        };
        let mut written = 0usize;
        let mut first_time_ticks = None;
        let mut overflow = self.pending_discontinuity;
        self.pending_discontinuity = false;
        if self.queue_gap.swap(false, Ordering::Relaxed) {
            overflow = true;
        }

        while written < buf.len() {
            // Fill the buffer like a hardware backend does. Returning whatever
            // is queued hands the receive chain blocks of a packet or two, and
            // its per-block work then outruns real time.
            let wait = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
            let Some((packet, consumed)) = self.next_packet(wait) else {
                break;
            };

            if consumed == 0 {
                let gap = packet.discontinuity
                    || self
                        .expected_seq
                        .map(|expected| packet.seq != expected)
                        .unwrap_or(false);
                self.expected_seq = Some(packet.seq.wrapping_add(1));
                if gap {
                    if written > 0 {
                        self.pending = Some((packet, 0));
                        self.pending_discontinuity = true;
                        break;
                    }
                    overflow = true;
                }
            }

            if first_time_ticks.is_none() {
                first_time_ticks = Some(packet.time_ticks.saturating_add(
                    crate::network::wire::samples_to_ticks(
                        consumed as u64,
                        self.clock.tick_rate(),
                        self.rx_sample_rate_hz,
                    ),
                ));
            }
            let take = (packet.samples.len() - consumed).min(buf.len() - written);
            buf[written..written + take]
                .copy_from_slice(&packet.samples[consumed..consumed + take]);
            written += take;
            if consumed + take < packet.samples.len() {
                self.pending = Some((packet, consumed + take));
            }
        }

        Ok(RxReadResult {
            samples_read: written,
            time_ticks: first_time_ticks.unwrap_or_else(|| self.clock.now_ticks()),
            overflow,
        })
    }

    fn rx_activate(&mut self, time_ticks: Option<u64>) -> Result<(), Error> {
        let minimum_time_ticks = time_ticks.unwrap_or_else(|| self.clock.now_ticks());
        let mut client = self.client.clone();
        self.rpc(
            "rx_activate",
            client.rx_activate(proto::RxActivateRequest { time_ticks }),
        )?;
        self.expected_seq = None;
        self.pending = None;
        // UHD may return buffered samples from before the previous stream stopped.
        self.minimum_time_ticks = Some(minimum_time_ticks);
        self.pending_discontinuity = false;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), Error> {
        let mut client = self.client.clone();
        self.rpc("rx_stop", client.rx_stop(()))?;
        Ok(())
    }

    fn set_rx_frequency(&mut self, frequency_hz: f64) -> Result<(), Error> {
        let mut client = self.client.clone();
        self.rpc(
            "set_rx_frequency",
            client.set_rx_frequency(proto::SetRxFrequencyRequest { frequency_hz }),
        )?;
        self.minimum_time_ticks = Some(self.clock.now_ticks());
        self.pending = None;
        self.expected_seq = None;
        self.pending_discontinuity = true;
        Ok(())
    }

    fn set_rx_gain(&mut self, gain_db: f64) -> Result<(), Error> {
        let mut client = self.client.clone();
        crate::stream::finite(gain_db, "RX gain")?;
        self.rpc(
            "set_rx_gain",
            client.set_rx_gain(proto::SetRxGainRequest { gain_db }),
        )?;
        let mut client = self.client.clone();
        let info = self.rpc("get_info", client.get_info(()))?;
        self.rx_reference_dbm = info.rx_reference_dbm.map(|reference| reference as f32);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
