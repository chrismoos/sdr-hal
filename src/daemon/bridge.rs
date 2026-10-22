use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::clock::{ClockSampler, device_time};
use super::composite::{RxFanout, Staged, TxGrid};
use super::sessions::Sessions;
use crate::network::wire::{
    self, FLAG_RX_DISCONTINUITY, FLAG_TX_BLOCK_END, FLAG_TX_DISABLE, FLAG_TX_ENABLE,
    FLAG_TX_NO_TICK, PacketHeader, StreamKind, decode_header, decode_samples_into, encode_packet,
};
use crate::{RadioRx, RadioTx, TxRadioHealth, error::Error};
use log::{debug, info, warn};
use num_complex::Complex32;
use parking_lot::Mutex;
use tokio::sync::oneshot;

const TX_SOCKET_POLL: Duration = Duration::from_micros(250);
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(100);
const RECV_BUF_LEN: usize = 65536;
const RX_READ_PACKETS: usize = 4;
const RX_READ_TIMEOUT_US: i64 = 100_000;
const IDLE_COMMAND_POLL: Duration = Duration::from_millis(100);
const TX_BLOCK_REORDER_GRACE: Duration = Duration::from_micros(500);
const TX_HEALTH_INTERVAL: Duration = Duration::from_millis(500);
/// Sample flow is the only client liveness signal. TX is disabled after this
/// much silence, so a dead client cannot leave the radio transmitting.
const TX_SILENCE_TIMEOUT: Duration = Duration::from_millis(500);
const TCP_ACCEPT_POLL: Duration = Duration::from_millis(20);
const RETURN_PATH_POLL: Duration = Duration::from_millis(1);
const MAX_TX_BLOCK_SAMPLES: usize = 1 << 20;
// Complete blocks may be submitted early. Missing client samples wait until the deadline.
const COMMIT_LEAD_BLOCKS: u64 = 6;
/// How often composite mode reads device time to decide what to commit.
const COMMIT_CLOCK_POLL: Duration = Duration::from_millis(1);
const TX_PACKET_QUEUE_CAPACITY: usize = 32_768;
const TX_PACKET_BATCH_LIMIT: usize = 256;
const COMPOSITE_TIMING_INTERVAL: Duration = Duration::from_secs(5);
/// Caps each composite emit pass, so control commands are still answered when synthesis
/// falls behind device time.
const COMPOSITE_EMIT_BUDGET: Duration = Duration::from_millis(20);
/// Lead past the composite deadline where an untimed stream starts.
const UNTIMED_START_MARGIN: Duration = Duration::from_millis(20);

fn block_id_is_newer(candidate: u32, reference: u32) -> bool {
    let forward = candidate.wrapping_sub(reference);
    forward != 0 && forward <= u32::MAX / 2
}

/// Commands for the TX worker. Each reply returns the driver result.
pub enum TxCommand {
    /// Replies with the TX clock in device ticks.
    GetHardwareTime(oneshot::Sender<Result<u64, Error>>),
    /// Resets the backend clock.
    SetHardwareTime {
        /// New device-clock value in ticks.
        time_ticks: u64,
        /// Receives the result when the driver operation finishes.
        reply: oneshot::Sender<Result<(), Error>>,
    },
    /// Enables or stops TX with an optional server-time request.
    EnableTransmit {
        /// True enables output. False stops output.
        enable: bool,
        /// Device time in ticks. `None` acts immediately.
        time_ticks: Option<u64>,
        /// Composite slot this applies to. In composite mode the device keeps
        /// transmitting while any other slot is enabled.
        slot: Option<usize>,
        /// Receives the result when the driver operation finishes.
        reply: oneshot::Sender<Result<(), Error>>,
    },
    /// Adds a composite slot and replies with its index.
    AddSlot {
        /// Rate ratio, frequency offset and filter for the new client.
        plan: crate::daemon::composite::SlicePlan,
        /// Linear amplitude this slice is summed at.
        gain: f32,
        /// Client sample rate in Hz, for fragment timestamp reconstruction.
        sample_rate_hz: f64,
        /// Receives the slot index, or why the slot could not be added.
        reply: oneshot::Sender<Result<usize, Error>>,
    },
    /// Replaces one composite slot's slice, which is how a client retunes its carrier.
    RetuneSlot {
        /// Slot to replace.
        slot: usize,
        /// Replacement rate ratio, frequency offset and filter.
        plan: crate::daemon::composite::SlicePlan,
        /// Linear amplitude this slice is summed at.
        gain: f32,
        /// Receives the result when the slice is in place.
        reply: oneshot::Sender<Result<(), Error>>,
    },
    /// Retunes the TX stream in Hz.
    SetFrequencyHz {
        /// Requested stream center frequency in Hz.
        frequency_hz: f64,
        /// Receives the result when the driver operation finishes.
        reply: oneshot::Sender<Result<(), Error>>,
    },
    /// Sets TX gain in dB and replies with the applied gain and the TX power estimate at it.
    SetGain {
        /// Requested TX gain in dB.
        gain_db: f64,
        /// Receives the applied gain and estimate when the driver operation finishes.
        reply: oneshot::Sender<Result<(f64, Option<f32>), Error>>,
    },
    /// Stops the owned stream, replies with its result, then exits the worker.
    Shutdown {
        /// Receives the result when the driver operation finishes.
        reply: oneshot::Sender<Result<(), Error>>,
    },
}

/// Commands for the RX worker. Each reply returns the driver result.
pub enum RxCommand {
    /// Starts RX with an optional server-time request. Native backends may start immediately.
    Activate {
        /// Device time in ticks. `None` acts immediately.
        time_ticks: Option<u64>,
        /// Receives the result when the driver operation finishes.
        reply: oneshot::Sender<Result<(), Error>>,
    },
    /// Stops RX while retaining the worker for later activation.
    Stop {
        /// Receives the result when the driver operation finishes.
        reply: oneshot::Sender<Result<(), Error>>,
    },
    /// Adds a composite slot and replies with its index.
    AddSlot {
        /// Rate ratio, frequency offset and filter for the new client.
        plan: crate::daemon::composite::SlicePlan,
        /// Client sample rate in Hz, for stepping timestamps within a block.
        sample_rate_hz: f64,
        /// Receives the slot index, or why the slot could not be added.
        reply: oneshot::Sender<Result<usize, Error>>,
    },
    /// Replaces one composite slot's slice, which is how a client retunes its carrier.
    RetuneSlot {
        /// Slot to replace.
        slot: usize,
        /// Replacement rate ratio, frequency offset and filter.
        plan: crate::daemon::composite::SlicePlan,
        /// Receives the result when the slice is in place.
        reply: oneshot::Sender<Result<(), Error>>,
    },
    /// Releases a composite slot whose client stopped receiving, freeing it for reuse.
    RetireSlot {
        /// Slot to release.
        slot: usize,
        /// Receives the result once the slot is free.
        reply: oneshot::Sender<Result<(), Error>>,
    },
    /// Retunes the RX stream in Hz.
    SetFrequency {
        /// Requested stream center frequency in Hz.
        frequency_hz: f64,
        /// Receives the result when the driver operation finishes.
        reply: oneshot::Sender<Result<(), Error>>,
    },
    /// Sets RX gain in dB.
    SetGain {
        /// Requested RX hardware gain in dB.
        gain_db: f64,
        /// Receives the result when the driver operation finishes.
        reply: oneshot::Sender<Result<Option<f32>, Error>>,
    },
    /// Stops the owned stream, replies with its result, then exits the worker.
    Shutdown {
        /// Receives the result when the driver operation finishes.
        reply: oneshot::Sender<Result<(), Error>>,
    },
}

/// A reassembled TX block. Missing fragments are filled with silence.
#[derive(Debug)]
pub struct CompletedTxBlock {
    /// Wrapping TX-block identifier from the packet header.
    pub block_id: u32,
    /// Full block in time order, with missing sample positions filled by zeros.
    pub samples: Vec<Complex32>,
    /// First-sample time in device ticks, or no timestamp for immediate output.
    pub time_ticks: Option<u64>,
    /// Number of positions filled by silence because no fragment supplied them.
    pub missing_samples: usize,
    pub(crate) end_of_burst: bool,
}

struct PendingTxBlock {
    block_id: u32,
    samples: Vec<Complex32>,
    received: Vec<bool>,
    received_count: usize,
    end_of_burst: bool,
    time_ticks: Option<u64>,
    end_seen_at: Option<Instant>,
    last_packet_at: Instant,
}

impl PendingTxBlock {
    fn finish(self) -> CompletedTxBlock {
        CompletedTxBlock {
            block_id: self.block_id,
            missing_samples: self.samples.len().saturating_sub(self.received_count),
            samples: self.samples,
            time_ticks: self.time_ticks,
            end_of_burst: self.end_of_burst,
        }
    }
}

#[derive(Default)]
struct TxStreamDiag {
    last_seq: Option<u32>,
    lost_packets: u64,
    late_packets: u64,
    reordered_packets: u64,
}

impl TxStreamDiag {
    fn observe(&mut self, seq: u32) {
        if let Some(previous) = self.last_seq {
            let advance = seq.wrapping_sub(previous);
            if advance == 0 || advance > u32::MAX / 2 {
                self.reordered_packets += 1;
                return;
            }
            self.lost_packets += u64::from(advance - 1);
        }
        self.last_seq = Some(seq);
    }

    fn note_late(&mut self) {
        self.late_packets += 1;
    }

    fn summary(&self) -> String {
        format!(
            "lost={} late={} reordered={}",
            self.lost_packets, self.late_packets, self.reordered_packets
        )
    }
}

/// Reassembles one pending TX block, accepting reordered fragments and ignoring duplicates.
/// Block IDs use wrapping half-range ordering. Newer blocks finish the prior block
/// with silence in missing positions. Call [`Self::reset`] at a session boundary.
#[derive(Default)]
pub struct TxBlockAssembler {
    pending: Option<PendingTxBlock>,
    last_completed_id: Option<u32>,
}

impl TxBlockAssembler {
    /// Discards pending samples and completed-block ordering history.
    pub fn reset(&mut self) {
        self.pending = None;
        self.last_completed_id = None;
    }

    /// Accepts a fragment and returns any finished blocks in order. A newer block can
    /// finish the pending block with silence. Duplicate positions keep their first sample.
    /// Late fragments are ignored. `tick_rate` and `sample_rate_hz` convert fragment
    /// time to block-start time.
    /// Returns errors for zero or excessive block lengths, out-of-range fragments, or changing totals.
    pub fn push(
        &mut self,
        header: &PacketHeader,
        packet_samples: &[Complex32],
        tick_rate: u64,
        sample_rate_hz: f64,
        now: Instant,
    ) -> Result<Vec<CompletedTxBlock>, Error> {
        let total = header.block_sample_count as usize;
        let offset = header.block_offset as usize;
        if total == 0 || total > MAX_TX_BLOCK_SAMPLES {
            return Err(format!("invalid TX block sample count {total}").into());
        }
        if offset
            .checked_add(packet_samples.len())
            .is_none_or(|end| end > total)
        {
            return Err(format!(
                "TX block range {}..{} exceeds total {total}",
                offset,
                offset.saturating_add(packet_samples.len())
            )
            .into());
        }

        let mut completed = Vec::new();
        if self.pending.as_ref().map(|block| block.block_id) != Some(header.block_id) {
            if let Some(pending) = &self.pending
                && !block_id_is_newer(header.block_id, pending.block_id)
            {
                return Ok(completed);
            }
            if let Some(last) = self.last_completed_id
                && !block_id_is_newer(header.block_id, last)
            {
                return Ok(completed);
            }
            if let Some(pending) = self.pending.take() {
                self.last_completed_id = Some(pending.block_id);
                completed.push(pending.finish());
            }
            let time_ticks = if header.flags & FLAG_TX_NO_TICK != 0 {
                None
            } else {
                Some(
                    header
                        .time_ticks
                        .saturating_sub(crate::network::wire::samples_to_ticks(
                            offset as u64,
                            tick_rate,
                            sample_rate_hz,
                        )),
                )
            };
            self.pending = Some(PendingTxBlock {
                block_id: header.block_id,
                samples: vec![Complex32::default(); total],
                received: vec![false; total],
                received_count: 0,
                end_of_burst: false,
                time_ticks,
                end_seen_at: None,
                last_packet_at: now,
            });
        }

        let pending = self.pending.as_mut().expect("TX block created above");
        if pending.samples.len() != total {
            return Err(format!(
                "TX block {} total changed from {} to {total}",
                header.block_id,
                pending.samples.len()
            )
            .into());
        }
        pending.last_packet_at = now;
        pending.end_of_burst |= header.flags & crate::network::wire::FLAG_TX_BURST_END != 0;
        for (index, sample) in packet_samples.iter().enumerate() {
            let target = offset + index;
            if !pending.received[target] {
                pending.received[target] = true;
                pending.received_count += 1;
                pending.samples[target] = *sample;
            }
        }
        if header.flags & FLAG_TX_BLOCK_END != 0 {
            pending.end_seen_at = Some(now);
        }
        if pending.received_count == pending.samples.len() {
            let pending = self.pending.take().expect("complete TX block");
            self.last_completed_id = Some(pending.block_id);
            completed.push(pending.finish());
        }
        Ok(completed)
    }

    /// Whether a block ID precedes or repeats the pending/completed ID under wrapping ordering.
    pub fn is_late(&self, block_id: u32) -> bool {
        if self.pending.as_ref().map(|block| block.block_id) == Some(block_id) {
            return false;
        }
        if let Some(pending) = &self.pending
            && !block_id_is_newer(block_id, pending.block_id)
        {
            return true;
        }
        if let Some(last) = self.last_completed_id
            && !block_id_is_newer(block_id, last)
        {
            return true;
        }
        false
    }

    /// Returns `(block_id, missing_sample_count)` for the pending block.
    pub fn pending_shortfall(&self) -> Option<(u32, usize)> {
        self.pending
            .as_ref()
            .map(|block| (block.block_id, block.samples.len() - block.received_count))
    }

    /// Finishes a block with silence after its end fragment has waited 500 microseconds.
    /// `now` must not precede the recorded fragment receipt time.
    pub fn expire(&mut self, now: Instant) -> Option<CompletedTxBlock> {
        let expired = self
            .pending
            .as_ref()
            .and_then(|block| block.end_seen_at)
            .is_some_and(|end_seen| now.duration_since(end_seen) >= TX_BLOCK_REORDER_GRACE);
        if !expired {
            return None;
        }
        let pending = self.pending.take().expect("expired TX block");
        self.last_completed_id = Some(pending.block_id);
        Some(pending.finish())
    }

    /// Discards a block lacking an end fragment after 500 ms without a fragment.
    /// Returns `(block_id, missing_sample_count)`. `now` must not precede the last receipt.
    pub fn drop_stalled(&mut self, now: Instant) -> Option<(u32, usize)> {
        let stalled = self.pending.as_ref().is_some_and(|block| {
            block.end_seen_at.is_none()
                && now.duration_since(block.last_packet_at) >= TX_SILENCE_TIMEOUT
        });
        if !stalled {
            return None;
        }
        self.discard_pending()
    }

    /// Discards the pending block and returns `(block_id, missing_sample_count)`.
    /// Later fragments of that block are treated as late.
    pub fn discard_pending(&mut self) -> Option<(u32, usize)> {
        let shortfall = self.pending_shortfall()?;
        self.pending = None;
        self.last_completed_id = Some(shortfall.0);
        Some(shortfall)
    }
}

#[derive(Default)]
struct TxSilenceWatchdog {
    last_activity: Option<Instant>,
}

impl TxSilenceWatchdog {
    fn arm(&mut self, now: Instant) {
        self.last_activity = Some(now);
    }

    fn disarm(&mut self) {
        self.last_activity = None;
    }

    fn note_samples(&mut self, now: Instant) {
        if let Some(last) = self.last_activity.as_mut() {
            *last = now;
        }
    }

    fn expired(&self, now: Instant) -> Option<Duration> {
        self.last_activity
            .map(|last| now.duration_since(last))
            .filter(|silent| *silent >= TX_SILENCE_TIMEOUT)
    }
}

/// Where the daemon sends RX IQ and clock heartbeats. A hello packet establishes
/// it on the transport it arrived over, so both directions share one transport.
pub enum ReturnPath {
    Udp(SocketAddr),
    Tcp(std::net::TcpStream),
}

impl ReturnPath {
    fn send(&mut self, socket: &UdpSocket, packet: &[u8]) -> std::io::Result<()> {
        match self {
            Self::Udp(peer) => socket.send_to(packet, *peer).map(|_| ()),
            Self::Tcp(stream) => std::io::Write::write_all(stream, packet),
        }
    }
}

pub type SharedReturnPath = Arc<Mutex<Option<ReturnPath>>>;

/// A TCP reader registers its own return path, so only the UDP variant stores
/// an address to answer on.
#[derive(Clone, Copy)]
enum PacketSource {
    Udp(SocketAddr),
    Tcp,
}

/// Settings for the TX worker. Both clock and sample rates must be positive.
pub struct TxBridge {
    /// Open sessions and the grid slot each one feeds.
    pub sessions: Arc<Sessions>,
    /// Native hardware ticks per second for timestamp conversion.
    pub tick_rate: u64,
    /// Applied TX sample rate in Hz per slot, for fragment timestamp reconstruction.
    /// Serving one client at a time holds a single entry.
    pub slot_rates: Vec<f64>,
    /// TCP IQ listener on the data port, shared with the session service.
    pub tcp_listener: Arc<std::net::TcpListener>,
    /// Composite summing grid. Absent writes each client's block straight to the driver.
    pub grid: Option<TxGrid>,
    /// Lead before playout at which incomplete composite output is committed.
    pub composite_deadline: Duration,
    /// Shared device sample rate, for a slot added after the grid started.
    pub device_rate_hz: u64,
}

/// Per-slot reassembly state. Each client reassembles and paces independently, so one
/// going quiet cannot discard another's pending block.
struct TxSlotState {
    assembler: TxBlockAssembler,
    diag: TxStreamDiag,
    sample_rate_hz: f64,
    watchdog: TxSilenceWatchdog,
    /// Whether this client asked to transmit. The device runs while any slot has.
    enabled: bool,
}

impl TxSlotState {
    fn new(sample_rate_hz: f64) -> Self {
        Self {
            assembler: TxBlockAssembler::default(),
            diag: TxStreamDiag::default(),
            sample_rate_hz,
            watchdog: TxSilenceWatchdog::default(),
            enabled: false,
        }
    }
}

/// Logs what staging did with a client block, since a grid slot that already went out
/// cannot be filled retrospectively.
fn log_staged(slot: usize, block_id: u32, outcome: Staged) {
    match outcome {
        Staged::Placed { rounded_samples: 0 } => {}
        Staged::Placed { rounded_samples } => debug!(
            "radiod: composite slot {slot} block {block_id} aligned {rounded_samples} block timestamp(s) to the nearest client sample"
        ),
        Staged::Late { blocks_behind } => warn!(
            "radiod: composite slot {slot} block {block_id} arrived {blocks_behind} grid block(s) after its place went out, dropping it"
        ),
        Staged::TooFarAhead { blocks_ahead } => warn!(
            "radiod: composite slot {slot} block {block_id} is scheduled {blocks_ahead} grid block(s) ahead, past the staging window, dropping it"
        ),
    }
}

fn submit_tx_block(tx: &mut dyn RadioTx, block: CompletedTxBlock, diag: &TxStreamDiag) {
    if block.missing_samples > 0 {
        warn!(
            "radiod: TX block {} missing {} of {} samples ({}); zero-filled before driver write",
            block.block_id,
            block.missing_samples,
            block.samples.len(),
            diag.summary()
        );
    }
    let schedule = block
        .time_ticks
        .map_or(crate::Schedule::Immediate, |ticks| {
            crate::Schedule::At(crate::DeviceTime::from_ticks(ticks))
        });
    let result = tx.write_all(
        &block.samples,
        crate::TxMetadata {
            schedule,
            end_of_burst: block.end_of_burst,
        },
    );

    if let Err(e) = result {
        warn!("radiod: transmit block {} failed: {e}", block.block_id);
    }
}

fn report_tx_health(tx: &mut dyn RadioTx, last: &mut TxRadioHealth) {
    let now = match tx.tx_health() {
        Ok(health) => health,
        Err(e) => {
            debug!("radiod: TX status unavailable: {e}");
            return;
        }
    };
    let late = now.late_packets.saturating_sub(last.late_packets);
    let underflows = now.underflows.saturating_sub(last.underflows);
    let sequence_errors = now.sequence_errors.saturating_sub(last.sequence_errors);
    let dropped = now.dropped_packets.saturating_sub(last.dropped_packets);
    let unknown = now.unknown_events.saturating_sub(last.unknown_events);
    if late > 0 {
        warn!(
            "radiod: inner radio discarded {late} late TX burst(s), total {} \
             (underflows {}, sequence errors {})",
            now.late_packets, now.underflows, now.sequence_errors
        );
    } else if underflows > 0 || sequence_errors > 0 || dropped > 0 || unknown > 0 {
        warn!(
            "radiod: TX status underflows +{underflows} sequence_errors +{sequence_errors} \
             dropped +{dropped} unknown +{unknown}"
        );
    }
    *last = now;
}

#[derive(Default)]
struct CompositeTxTiming {
    max_loop_gap: Duration,
    max_clock_query: Duration,
    max_clock_refresh: Duration,
    max_synthesis: Duration,
    max_write: Duration,
    max_packet_queue: Duration,
    max_packet_backlog: usize,
    min_estimated_write_lead_us: Option<f64>,
}

struct SocketReaderGuard(Arc<AtomicBool>);

impl Drop for SocketReaderGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

struct ReceivedPacket {
    bytes: Vec<u8>,
    source: PacketSource,
    received_at: Instant,
}

type PacketReceiver = crossbeam_channel::Receiver<ReceivedPacket>;

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
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => {
                warn!("radiod: TX data-plane TCP read failed: {e}");
                return false;
            }
        }
    }
    true
}

/// Reads one client's packets and returns the session it served, so the accept loop can
/// end that session when the client goes away. A TCP data stream stays open for the
/// client's whole life, so losing it is a definitive report that the client left.
fn read_tcp_packets(
    mut stream: std::net::TcpStream,
    peer: SocketAddr,
    sessions: &Sessions,
    packets: &crossbeam_channel::Sender<ReceivedPacket>,
    stop: &AtomicBool,
) -> Option<(u32, SharedReturnPath)> {
    let mut registered: Option<(u32, SharedReturnPath)> = None;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        buf.resize(wire::HEADER_LEN, 0);
        if !read_fully(&mut stream, &mut buf, stop) {
            return registered;
        }
        let payload = match wire::payload_len(&buf) {
            Ok(payload) => payload,
            Err(e) => {
                warn!("radiod: TX data-plane TCP framing lost: {e}");
                return registered;
            }
        };
        buf.resize(wire::HEADER_LEN + payload, 0);
        if !read_fully(&mut stream, &mut buf[wire::HEADER_LEN..], stop) {
            return registered;
        }
        let header = match decode_header(&buf) {
            Ok(header) => header,
            Err(e) => {
                warn!("radiod: TX data-plane TCP framing lost: {e}");
                return registered;
            }
        };
        let Some(session) = sessions.find(header.session_id) else {
            continue;
        };
        if header.stream == StreamKind::Hello {
            match stream.try_clone() {
                Ok(writer) => {
                    info!(
                        "radiod: data plane over TCP from {peer} for session {:#010x}",
                        session.session_id
                    );
                    *session.peer.lock() = Some(ReturnPath::Tcp(writer));
                    registered = Some((session.session_id, session.peer.clone()));
                }
                Err(e) => {
                    warn!("radiod: cannot answer TCP client {peer}: {e}");
                    return registered;
                }
            }
        } else if header.stream != StreamKind::TxSamples {
            continue;
        }
        if packets
            .send(ReceivedPacket {
                bytes: buf.clone(),
                source: PacketSource::Tcp,
                received_at: Instant::now(),
            })
            .is_err()
        {
            return registered;
        }
    }
}

fn spawn_tcp_reader(
    listener: Arc<std::net::TcpListener>,
    sessions: Arc<Sessions>,
    packets: crossbeam_channel::Sender<ReceivedPacket>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<()> {
    listener.set_nonblocking(true)?;
    std::thread::Builder::new()
        .name("radiod-tx-tcp".into())
        .spawn(move || {
            loop {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                match listener.accept() {
                    Ok((stream, peer)) => {
                        if stream.set_nonblocking(false).is_err() {
                            continue;
                        }
                        let _ = stream.set_nodelay(true);
                        let _ = stream.set_read_timeout(Some(TCP_ACCEPT_POLL));
                        // Each client gets its own reader, so the accept loop stays free.
                        // Reading inline would block it for the first client's whole
                        // life, and a composite peer's data plane would never connect.
                        let sessions = Arc::clone(&sessions);
                        let packets = packets.clone();
                        let stop = Arc::clone(&stop);
                        let spawned = std::thread::Builder::new()
                            .name("radiod-tx-tcp-client".into())
                            .spawn(move || {
                                let registered =
                                    read_tcp_packets(stream, peer, &sessions, &packets, &stop);
                                info!("radiod: data-plane TCP client {peer} went away");
                                if let Some((session_id, return_path)) = registered {
                                    {
                                        let mut path = return_path.lock();
                                        if matches!(*path, Some(ReturnPath::Tcp(_))) {
                                            *path = None;
                                        }
                                    }
                                    // Ending the session releases the slots it held, which
                                    // both stream workers then reclaim.
                                    sessions.remove(session_id);
                                }
                            });
                        if let Err(e) = spawned {
                            warn!("radiod: cannot serve TCP data-plane client {peer}: {e}");
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(TCP_ACCEPT_POLL);
                    }
                    Err(e) => {
                        warn!("radiod: TX data-plane accept failed: {e}");
                        return;
                    }
                }
            }
        })?;
    Ok(())
}

fn spawn_socket_reader(
    socket: &UdpSocket,
) -> std::io::Result<(
    PacketReceiver,
    crossbeam_channel::Sender<ReceivedPacket>,
    SocketReaderGuard,
)> {
    let stop = Arc::new(AtomicBool::new(false));
    let guard = SocketReaderGuard(Arc::clone(&stop));
    let socket = socket.try_clone()?;
    socket.set_read_timeout(Some(IDLE_COMMAND_POLL))?;
    let (tx, rx) = crossbeam_channel::bounded(TX_PACKET_QUEUE_CAPACITY);
    let shared_sender = tx.clone();
    std::thread::Builder::new()
        .name("radiod-tx-recv".into())
        .spawn(move || {
            let mut buf = vec![0u8; RECV_BUF_LEN];
            let mut dropped: u64 = 0;
            loop {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let (len, from) = match socket.recv_from(&mut buf) {
                    Ok(ok) => ok,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        continue;
                    }
                    Err(e) => {
                        warn!("radiod: data socket recv failed: {e}");
                        return;
                    }
                };
                match tx.try_send(ReceivedPacket {
                    bytes: buf[..len].to_vec(),
                    source: PacketSource::Udp(from),
                    received_at: Instant::now(),
                }) {
                    Ok(()) => {}
                    Err(crossbeam_channel::TrySendError::Full(_)) => {
                        dropped += 1;
                        if dropped.is_power_of_two() {
                            warn!("radiod: TX packet queue full, dropped {dropped} packets total");
                        }
                    }
                    Err(crossbeam_channel::TrySendError::Disconnected(_)) => return,
                }
            }
        })?;
    Ok((rx, shared_sender, guard))
}

impl TxBridge {
    /// Owns TX and processes commands and IQ until shutdown or command-channel closure.
    /// Disables TX after 500 ms without samples. Blocks the calling thread.
    pub fn run(
        mut self,
        mut tx: Box<dyn RadioTx>,
        socket: UdpSocket,
        cmds: crossbeam_channel::Receiver<TxCommand>,
    ) {
        let (packets, packet_sender, _reader_guard) = match spawn_socket_reader(&socket) {
            Ok(reader) => reader,
            Err(e) => {
                warn!("radiod: tx bridge could not start its socket reader: {e}");
                return;
            }
        };
        let tcp_stop = Arc::new(AtomicBool::new(false));
        let _tcp_guard = SocketReaderGuard(Arc::clone(&tcp_stop));
        if let Err(e) = spawn_tcp_reader(
            Arc::clone(&self.tcp_listener),
            Arc::clone(&self.sessions),
            packet_sender,
            Arc::clone(&tcp_stop),
        ) {
            warn!("radiod: tx bridge could not accept stream clients: {e}");
        }
        let clock_cache = match self.grid.as_ref().and_then(|_| tx.clock_reader()) {
            Some(reader) => match ClockSampler::start(reader, self.tick_rate) {
                Ok(cache) => Some(cache),
                Err(error) => {
                    warn!("radiod: cannot initialize cached device clock: {error}");
                    let _ = tx.stop();
                    return;
                }
            },
            None => None,
        };
        let mut samples: Vec<Complex32> = Vec::new();
        let mut scratch: Vec<u8> = Vec::new();
        let mut slots: Vec<TxSlotState> = self
            .slot_rates
            .iter()
            .map(|&rate| TxSlotState::new(rate))
            .collect();
        let mut last_generation = self.sessions.generation();
        let mut last_heartbeat = Instant::now() - HEARTBEAT_INTERVAL;
        let mut heartbeat_seqs: std::collections::HashMap<u32, u32> =
            std::collections::HashMap::new();
        let mut last_health = TxRadioHealth::default();
        let mut last_health_poll = Instant::now();
        let mut last_commit_poll = Instant::now() - COMMIT_CLOCK_POLL;
        let mut commit_block: u64 = 0;
        let mut deadline_block: u64 = 0;
        let mut device_enabled = false;
        let mut grid_was_enabled = false;
        let mut slot_losses: Vec<(u64, u64, u64)> = Vec::new();
        let mut retired: Vec<usize> = Vec::new();
        let mut timing = CompositeTxTiming::default();
        let mut last_timing_report = Instant::now();
        let mut last_loop = Instant::now();
        let mut clock_anchor = None;
        if slots.is_empty() {
            slots.push(TxSlotState::new(0.0));
        }

        loop {
            let loop_start = Instant::now();
            timing.max_loop_gap = timing
                .max_loop_gap
                .max(loop_start.duration_since(last_loop));
            last_loop = loop_start;
            timing.max_packet_backlog = timing.max_packet_backlog.max(packets.len());
            loop {
                match cmds.try_recv() {
                    Ok(TxCommand::GetHardwareTime(reply)) => {
                        let _ = reply.send(device_time(&*tx, clock_cache.as_ref()));
                    }
                    Ok(TxCommand::SetHardwareTime { time_ticks, reply }) => {
                        let _ = reply.send(tx.set_hardware_time(time_ticks));
                    }
                    Ok(TxCommand::EnableTransmit {
                        enable,
                        time_ticks,
                        slot,
                        reply,
                    }) => {
                        let index = slot.filter(|&index| index < slots.len()).unwrap_or(0);
                        if self.grid.is_some() {
                            // One client stopping must not mute its peers, so the device
                            // only follows the whole set of slots.
                            if index < slots.len() {
                                slots[index].enabled = enable;
                                if enable {
                                    slots[index].watchdog.arm(Instant::now());
                                } else {
                                    slots[index].watchdog.disarm();
                                    if let Some(grid) = self.grid.as_mut() {
                                        grid.silence_slot(index);
                                    }
                                }
                            }
                            let want = slots.iter().any(|slot| slot.enabled);
                            let result = if want == device_enabled {
                                Ok(())
                            } else {
                                let result = tx.enable_transmit_at(want, time_ticks);
                                if result.is_ok() {
                                    device_enabled = want;
                                }
                                result
                            };
                            let _ = reply.send(result);
                        } else {
                            let result = tx.enable_transmit_at(enable, time_ticks);
                            if !enable {
                                slots[index].watchdog.disarm();
                            } else if result.is_ok() {
                                slots[index].watchdog.arm(Instant::now());
                            }
                            if result.is_ok() {
                                slots[index].enabled = enable;
                                device_enabled = enable;
                            }
                            let _ = reply.send(result);
                        }
                    }
                    Ok(TxCommand::AddSlot {
                        plan,
                        gain,
                        sample_rate_hz,
                        reply,
                    }) => {
                        let result = match &mut self.grid {
                            Some(grid) => {
                                let index = grid.add_slot(&plan, self.device_rate_hz, gain);
                                while slots.len() <= index {
                                    slots.push(TxSlotState::new(sample_rate_hz));
                                }
                                slots[index].sample_rate_hz = sample_rate_hz;
                                slot_losses.resize(grid.slots_added(), (0, 0, 0));
                                info!(
                                    "radiod: composite TX slot {index} at {:+} Hz, {}/{} device samples per client sample",
                                    plan.offset_hz, plan.ratio, plan.denominator
                                );
                                Ok(index)
                            }
                            None => Err(Error::from(
                                "this daemon serves one client at a time, so it has no composite slots",
                            )),
                        };
                        let _ = reply.send(result);
                    }
                    Ok(TxCommand::RetuneSlot {
                        slot,
                        plan,
                        gain,
                        reply,
                    }) => {
                        let device_rate_hz = self.device_rate_hz;
                        let result = match &mut self.grid {
                            Some(grid) => grid
                                .retune_slot(slot, &plan, device_rate_hz, gain)
                                .map(|()| {
                                    info!(
                                        "radiod: composite TX slot {slot} retuned to {:+} Hz from center",
                                        plan.offset_hz
                                    );
                                }),
                            None => Err(Error::from(
                                "this daemon serves one client at a time, so it has no composite slots",
                            )),
                        };
                        let _ = reply.send(result);
                    }
                    Ok(TxCommand::SetFrequencyHz {
                        frequency_hz,
                        reply,
                    }) => {
                        let result = tx.set_tx_frequency_hz(frequency_hz);
                        let _ = reply.send(result);
                    }
                    Ok(TxCommand::SetGain { gain_db, reply }) => {
                        let result = tx
                            .set_tx_gain(gain_db)
                            .map(|applied| (applied, tx.tx_full_scale_power_estimate_dbm()));
                        let _ = reply.send(result);
                    }
                    Ok(TxCommand::Shutdown { reply }) => {
                        let _ = reply.send(tx.stop());
                        return;
                    }
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        if let Err(error) = tx.stop() {
                            warn!("radiod: could not stop TX after control disconnect: {error}");
                        }
                        return;
                    }
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                }
            }

            for packet_index in 0..TX_PACKET_BATCH_LIMIT {
                let ReceivedPacket {
                    bytes: buf,
                    source: from,
                    received_at,
                } = match packets.recv_timeout(if packet_index != 0 {
                    Duration::ZERO
                } else if device_enabled && self.grid.is_some() {
                    COMMIT_CLOCK_POLL
                } else {
                    TX_SOCKET_POLL
                }) {
                    Ok(packet) => packet,
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => break,
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        warn!("radiod: data socket reader stopped");
                        return;
                    }
                };
                let queue_delay = received_at.elapsed();
                timing.max_packet_queue = timing.max_packet_queue.max(queue_delay);
                let len = buf.len();
                let header = match decode_header(&buf[..len]) {
                    Ok(header) => header,
                    Err(e) => {
                        debug!("radiod: dropping undecodable packet: {e}");
                        continue;
                    }
                };
                let Some(open) = self.sessions.find(header.session_id) else {
                    continue;
                };
                match header.stream {
                    StreamKind::Hello => {
                        // A TCP hello was already registered by its reader.
                        if let PacketSource::Udp(from) = from {
                            let mut path = open.peer.lock();
                            if !matches!(*path, Some(ReturnPath::Udp(current)) if current == from) {
                                info!(
                                    "radiod: data-plane client for session {:#010x} is {from}",
                                    open.session_id
                                );
                                *path = Some(ReturnPath::Udp(from));
                            }
                        }
                        last_heartbeat = Instant::now() - HEARTBEAT_INTERVAL;
                    }
                    StreamKind::TxSamples => {
                        let Some(index) = open.tx_slot.filter(|&index| index < slots.len()) else {
                            debug!(
                                "radiod: dropping TX samples from receive-only session {:#010x}",
                                open.session_id
                            );
                            continue;
                        };
                        slots[index].diag.observe(header.seq);
                        if header.flags & FLAG_TX_ENABLE != 0 {
                            if header.sample_count != 0 || header.flags & FLAG_TX_DISABLE != 0 {
                                warn!("radiod: dropping invalid TX enable fence");
                                continue;
                            }
                            let ticks =
                                (header.flags & FLAG_TX_NO_TICK == 0).then_some(header.time_ticks);
                            if !device_enabled {
                                if let Err(error) = tx.enable_transmit_at(true, ticks) {
                                    warn!("radiod: ordered TX enable failed: {error}");
                                    continue;
                                }
                                device_enabled = true;
                            }
                            slots[index].enabled = true;
                            slots[index].watchdog.arm(Instant::now());
                            info!("radiod: composite slot {index} ordered its slice active");
                            continue;
                        }
                        if header.flags & FLAG_TX_DISABLE != 0 {
                            if header.sample_count != 0 {
                                warn!(
                                    "radiod: dropping TX disable packet with {} samples",
                                    header.sample_count
                                );
                                continue;
                            }
                            if let Some(block) = slots[index].assembler.expire(Instant::now()) {
                                self.deliver(
                                    &mut *tx,
                                    clock_cache.as_ref(),
                                    index,
                                    block,
                                    &slots[index].diag,
                                );
                            }
                            if let Some((block_id, shortfall)) =
                                slots[index].assembler.discard_pending()
                            {
                                warn!(
                                    "radiod: TX disable reached incomplete block {block_id} ({shortfall} samples short), dropping it"
                                );
                            }
                            slots[index].watchdog.disarm();
                            slots[index].enabled = false;
                            if let Some(grid) = self.grid.as_mut() {
                                grid.silence_slot(index);
                                info!("radiod: composite slot {index} ordered its slice silent");
                                if device_enabled && !slots.iter().any(|slot| slot.enabled) {
                                    match tx.stop() {
                                        Ok(()) => device_enabled = false,
                                        Err(error) => warn!("radiod: TX disable failed: {error}"),
                                    }
                                }
                            } else if let Err(e) = tx.stop() {
                                warn!("radiod: ordered TX disable failed: {e}");
                            } else {
                                device_enabled = false;
                            }
                            if !device_enabled && let Some(grid) = self.grid.as_mut() {
                                grid.reset_timeline();
                                for slot in &mut slots {
                                    slot.assembler.reset();
                                }
                            }
                            continue;
                        }
                        slots[index].watchdog.note_samples(Instant::now());
                        if slots[index].assembler.is_late(header.block_id) {
                            slots[index].diag.note_late();
                            if let Some((pending_id, shortfall)) =
                                slots[index].assembler.pending_shortfall()
                            {
                                debug!(
                                    "radiod: TX packet seq {} for block {} arrived after that block went out, assembling {} ({} samples short)",
                                    header.seq, header.block_id, pending_id, shortfall
                                );
                            }
                        }
                        if decode_samples_into(&buf[..len], &header, &mut samples).is_err() {
                            continue;
                        }
                        let rate = slots[index].sample_rate_hz;
                        match slots[index].assembler.push(
                            &header,
                            &samples,
                            self.tick_rate,
                            rate,
                            Instant::now(),
                        ) {
                            Ok(blocks) => {
                                for block in blocks {
                                    self.deliver(
                                        &mut *tx,
                                        clock_cache.as_ref(),
                                        index,
                                        block,
                                        &slots[index].diag,
                                    );
                                }
                            }
                            Err(e) => warn!("radiod: dropping invalid TX packet: {e}"),
                        }
                    }
                    StreamKind::RxSamples | StreamKind::ClockHeartbeat => {}
                }
            }

            let now = Instant::now();
            for (index, slot) in slots.iter_mut().enumerate() {
                if let Some(block) = slot.assembler.expire(now) {
                    match &mut self.grid {
                        Some(grid) => stage_block(
                            grid,
                            index,
                            block,
                            untimed_earliest(
                                &*tx,
                                clock_cache.as_ref(),
                                self.tick_rate,
                                self.composite_deadline,
                            ),
                        ),
                        None => submit_tx_block(&mut *tx, block, &slot.diag),
                    }
                }
                if let Some((block_id, shortfall)) = slot.assembler.drop_stalled(now) {
                    warn!(
                        "radiod: dropped TX block {block_id} ({shortfall} samples short), no END packet within {} ms ({})",
                        TX_SILENCE_TIMEOUT.as_millis(),
                        slot.diag.summary()
                    );
                }
                if let Some(silent) = slot.watchdog.expired(now) {
                    slot.watchdog.disarm();
                    slot.enabled = false;
                    if self.grid.is_some() {
                        warn!(
                            "radiod: composite slot {index} sent no TX samples for {} ms, releasing its slice",
                            silent.as_millis()
                        );
                        retired.push(index);
                    } else {
                        warn!(
                            "radiod: no TX samples for {} ms while transmit enabled, disabling transmit",
                            silent.as_millis()
                        );
                        if let Err(e) = tx.stop() {
                            warn!("radiod: TX silence disable failed: {e}");
                        }
                        device_enabled = false;
                    }
                }
            }

            // A client that died mid-transmission must not leave the device radiating,
            // so the last slice going silent stops it just as it does for one client.
            for index in retired.drain(..) {
                if let Some(grid) = self.grid.as_mut()
                    && grid.retire_slot(index)
                {
                    info!("radiod: composite TX slot {index} released for reuse");
                }
                if device_enabled && !slots.iter().any(|slot| slot.enabled) {
                    warn!("radiod: every composite slice went silent, disabling transmit");
                    match tx.stop() {
                        Ok(()) => device_enabled = false,
                        Err(e) => warn!("radiod: TX silence disable failed: {e}"),
                    }
                }
            }

            if grid_was_enabled && !device_enabled {
                if let Some(grid) = self.grid.as_mut() {
                    grid.reset_timeline();
                }
                for slot in &mut slots {
                    slot.assembler.reset();
                }
            }
            grid_was_enabled = device_enabled;
            if let Some(grid) = self.grid.as_ref()
                && device_enabled
                && last_commit_poll.elapsed() >= COMMIT_CLOCK_POLL
            {
                last_commit_poll = Instant::now();
                let clock_query = Instant::now();
                match device_time(&*tx, clock_cache.as_ref()) {
                    Ok(ticks) => {
                        clock_anchor = Some((ticks, clock_query));
                        commit_block = grid.block_at_ticks(ticks) + COMMIT_LEAD_BLOCKS;
                        let deadline_lead_ticks = (self.tick_rate as u128
                            * self.composite_deadline.as_nanos()
                            / Duration::from_secs(1).as_nanos())
                            as u64;
                        deadline_block =
                            grid.block_at_ticks(ticks.saturating_add(deadline_lead_ticks));
                        commit_block = commit_block.max(deadline_block);
                    }
                    Err(e) => debug!("radiod: composite commit clock read failed: {e}"),
                }
                timing.max_clock_query = timing.max_clock_query.max(clock_query.elapsed());
                if let Some(cache) = clock_cache.as_ref() {
                    timing.max_clock_refresh =
                        timing.max_clock_refresh.max(cache.take_max_refresh());
                }
                let emit_started = Instant::now();
                while emit_started.elapsed() < COMPOSITE_EMIT_BUDGET {
                    let synthesis = Instant::now();
                    let Some(block) = self.grid.as_mut().and_then(|grid| {
                        grid.emit_available(commit_block, deadline_block, |index| {
                            slots[index].enabled
                        })
                    }) else {
                        break;
                    };
                    timing.max_synthesis = timing.max_synthesis.max(synthesis.elapsed());
                    if let Some((ticks, observed_at)) = clock_anchor {
                        let lead_us = (block.time_ticks as f64 - ticks as f64) * 1_000_000.0
                            / self.tick_rate as f64
                            - observed_at.elapsed().as_secs_f64() * 1_000_000.0;
                        timing.min_estimated_write_lead_us = Some(
                            timing
                                .min_estimated_write_lead_us
                                .map_or(lead_us, |previous| previous.min(lead_us)),
                        );
                    }
                    let write = Instant::now();
                    submit_tx_block(
                        &mut *tx,
                        CompletedTxBlock {
                            block_id: 0,
                            samples: block.samples,
                            time_ticks: Some(block.time_ticks),
                            missing_samples: 0,
                            end_of_burst: false,
                        },
                        &TxStreamDiag::default(),
                    );
                    timing.max_write = timing.max_write.max(write.elapsed());
                }
            }

            if self.grid.is_some() && last_timing_report.elapsed() >= COMPOSITE_TIMING_INTERVAL {
                info!(
                    "radiod: composite TX timing: max_loop_gap_us={} max_clock_query_us={} max_clock_refresh_us={} max_synthesis_us={} max_write_us={} max_packet_queue_us={} max_packet_backlog={} min_estimated_write_lead_us={:?}",
                    timing.max_loop_gap.as_micros(),
                    timing.max_clock_query.as_micros(),
                    timing.max_clock_refresh.as_micros(),
                    timing.max_synthesis.as_micros(),
                    timing.max_write.as_micros(),
                    timing.max_packet_queue.as_micros(),
                    timing.max_packet_backlog,
                    timing.min_estimated_write_lead_us
                );
                timing = CompositeTxTiming::default();
                last_timing_report = Instant::now();
            }

            if last_health_poll.elapsed() >= TX_HEALTH_INTERVAL {
                last_health_poll = Instant::now();
                report_tx_health(&mut *tx, &mut last_health);
                if let Some(grid) = self.grid.as_ref() {
                    for (index, last) in slot_losses.iter_mut().enumerate() {
                        let now = grid.slot_diagnostics(index);
                        if (now.0, now.1) != (last.0, last.1) {
                            warn!(
                                "radiod: composite slot {index} has missed {} block(s) too late, refused {} too far ahead, and aligned {} block timestamp(s)",
                                now.0, now.1, now.2
                            );
                        } else if now.2 != last.2 {
                            debug!(
                                "radiod: composite slot {index} aligned {} block timestamp(s) to the nearest client sample, with no additional staging drops",
                                now.2
                            );
                        }
                        *last = now;
                    }
                }
            }

            let generation = self.sessions.generation();
            if generation != last_generation {
                last_generation = generation;
                info!("radiod: {} session(s) open", self.sessions.len());
                // A superseded single client must not have its half-assembled block
                // handed to the session that replaced it. Composite slots keep their
                // state, since a peer joining says nothing about an existing slice.
                if self.grid.is_none() {
                    for slot in &mut slots {
                        slot.assembler.reset();
                        slot.diag = TxStreamDiag::default();
                    }
                    // An ended session's queued packets, its disable fence included, are dropped.
                    if device_enabled && self.sessions.len() == 0 {
                        info!("radiod: the transmitting session ended, disabling transmit");
                        slots[0].enabled = false;
                        slots[0].watchdog.disarm();
                        match tx.stop() {
                            Ok(()) => device_enabled = false,
                            Err(e) => warn!("radiod: TX disable failed: {e}"),
                        }
                    }
                } else {
                    for index in self.sessions.take_released_tx() {
                        if let Some(grid) = self.grid.as_mut()
                            && grid.retire_slot(index)
                        {
                            info!("radiod: composite TX slot {index} released, its session ended");
                        }
                        if let Some(slot) = slots.get_mut(index) {
                            slot.enabled = false;
                            slot.watchdog.disarm();
                            slot.assembler.reset();
                            slot.diag = TxStreamDiag::default();
                        }
                    }
                    if device_enabled && !slots.iter().any(|slot| slot.enabled) {
                        warn!("radiod: no composite slice wants to transmit, disabling transmit");
                        match tx.stop() {
                            Ok(()) => device_enabled = false,
                            Err(e) => warn!("radiod: TX disable failed: {e}"),
                        }
                    }
                }
                heartbeat_seqs.retain(|&id, _| self.sessions.accepts(id));
            }

            // Every client anchors its own clock, so each open session gets a
            // heartbeat. RX holds a peer lock across a blocking TCP write, so a slow
            // consumer must not stall TX: skip that session and retry next interval.
            if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
                let ticks = match device_time(&*tx, clock_cache.as_ref()) {
                    Ok(ticks) => Some(ticks),
                    Err(e) => {
                        debug!("radiod: heartbeat time read failed: {e}");
                        None
                    }
                };
                if let Some(ticks) = ticks {
                    for open in self.sessions.all() {
                        let Some(mut guard) = open.peer.try_lock() else {
                            continue;
                        };
                        let Some(path) = guard.as_mut() else { continue };
                        let seq = heartbeat_seqs.entry(open.session_id).or_insert(0);
                        if let Err(error) = encode_packet(
                            &PacketHeader {
                                stream: StreamKind::ClockHeartbeat,
                                flags: 0,
                                session_id: open.session_id,
                                seq: *seq,
                                time_ticks: ticks,
                                sample_count: 0,
                                block_id: 0,
                                block_offset: 0,
                                block_sample_count: 0,
                            },
                            &[],
                            &mut scratch,
                        ) {
                            warn!("radiod: cannot encode packet: {error}");
                            continue;
                        }
                        *seq = seq.wrapping_add(1);
                        if let Err(e) = path.send(&socket, &scratch) {
                            debug!("radiod: heartbeat send failed: {e}");
                        }
                    }
                }
                last_heartbeat = Instant::now();
            }
        }
    }

    /// Sends a finished client block to the grid, or straight to the driver when
    /// serving one client.
    fn deliver(
        &mut self,
        tx: &mut dyn RadioTx,
        clock: Option<&ClockSampler>,
        slot: usize,
        block: CompletedTxBlock,
        diag: &TxStreamDiag,
    ) {
        match &mut self.grid {
            Some(grid) => stage_block(
                grid,
                slot,
                block,
                untimed_earliest(tx, clock, self.tick_rate, self.composite_deadline),
            ),
            None => submit_tx_block(tx, block, diag),
        }
    }
}

/// Earliest tick an untimed composite block can start at, read only when needed.
fn untimed_earliest<'a>(
    tx: &'a dyn RadioTx,
    clock: Option<&'a ClockSampler>,
    tick_rate: u64,
    deadline: Duration,
) -> impl FnOnce() -> Result<u64, Error> + 'a {
    move || {
        let lead = deadline + UNTIMED_START_MARGIN;
        let lead_ticks =
            (tick_rate as u128 * lead.as_nanos() / Duration::from_secs(1).as_nanos()) as u64;
        device_time(tx, clock).map(|now| now.saturating_add(lead_ticks))
    }
}

/// Stages one client block on the grid. An untimed block continues the slot's untimed stream.
fn stage_block(
    grid: &mut TxGrid,
    slot: usize,
    block: CompletedTxBlock,
    untimed_earliest: impl FnOnce() -> Result<u64, Error>,
) {
    let outcome = match block.time_ticks {
        Some(time_ticks) => grid.stage(slot, time_ticks, &block.samples),
        None => match untimed_earliest() {
            Ok(earliest) => grid.stage_untimed(slot, earliest, &block.samples),
            Err(error) => {
                warn!(
                    "radiod: composite slot {slot} block {} has no timestamp and device time is unavailable, dropping it: {error}",
                    block.block_id
                );
                return;
            }
        },
    };
    log_staged(slot, block.block_id, outcome);
}

/// Commands for the RX worker beyond the stream controls, used by composite mode.
/// Settings for the RX worker. Clock rate and packet sample count must be positive.
pub struct RxBridge {
    /// Open sessions and the fan-out slot each one reads.
    pub sessions: Arc<Sessions>,
    /// Native hardware ticks per second for timestamp conversion.
    pub tick_rate: u64,
    /// Applied RX sample rate in Hz, updated by the session service. Composite mode
    /// takes each client's rate from `slot_rates` instead.
    pub rx_sample_rate_hz: Arc<AtomicU64>,
    /// Negotiated maximum complex samples per outgoing RX datagram.
    pub samples_per_packet: usize,
    /// Per-client slicing. Absent sends the device stream to the one open session.
    pub fanout: Option<RxFanout>,
    /// Client sample rate per slot, for stepping timestamps within a block.
    pub slot_rates: Vec<f64>,
    /// Device samples read per pass in composite mode, which keeps every slot's
    /// decimation phase aligned.
    pub block_samples: usize,
    /// Shared device sample rate, for a slot added after the fan-out started.
    pub device_rate_hz: u64,
}

impl RxBridge {
    /// Owns RX and sends IQ over the session's return path until shutdown or
    /// command-channel closure. Blocks the calling thread. Shutdown stops RX
    /// before replying.
    pub fn run(
        mut self,
        mut rx: Box<dyn RadioRx>,
        socket: UdpSocket,
        cmds: crossbeam_channel::Receiver<RxCommand>,
    ) {
        let read_len = if self.fanout.is_some() {
            self.block_samples
        } else {
            self.samples_per_packet * RX_READ_PACKETS
        };
        let mut buf = vec![Complex32::default(); read_len];
        let mut scratch: Vec<u8> = Vec::new();
        let mut seq: u32 = 0;
        let mut seqs: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
        let mut active = false;
        let mut delivered = false;
        let mut last_generation = self.sessions.generation();

        loop {
            if active {
                loop {
                    match cmds.try_recv() {
                        Ok(cmd) => {
                            if self.handle_command(cmd, &mut rx, &mut active) {
                                return;
                            }
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => break,
                        Err(crossbeam_channel::TryRecvError::Disconnected) => return,
                    }
                }
                if !active {
                    continue;
                }
                // Hold the first read until the client's return path exists, so the
                // stream starts where the device did. Later gaps keep draining, which
                // stops a client that went away from overflowing the device.
                let open = self.sessions.all();
                // A session that ended gives its slice back, which is what releases a
                // receive-only client's slot when its data stream drops.
                let generation = self.sessions.generation();
                if generation != last_generation {
                    last_generation = generation;
                    for index in self.sessions.take_released_rx() {
                        if let Some(fanout) = self.fanout.as_mut()
                            && fanout.retire_slot(index)
                        {
                            info!("radiod: composite RX slot {index} released, its session ended");
                        }
                    }
                    seqs.retain(|&id, _| open.iter().any(|open| open.session_id == id));
                }
                if !delivered && !open.iter().any(|session| session.peer.lock().is_some()) {
                    std::thread::sleep(RETURN_PATH_POLL);
                    continue;
                }
                let result = match rx.rx_read(&mut buf, RX_READ_TIMEOUT_US) {
                    Ok(result) => result,
                    Err(e) => {
                        warn!("radiod: rx_read failed: {e}");
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                };
                if result.samples_read == 0 {
                    continue;
                }
                if let Some(fanout) = self.fanout.as_mut() {
                    // Every client reads the same device block through its own slice,
                    // so one slow consumer never reorders another's stream.
                    for session in &open {
                        let Some(index) = session.rx_slot else {
                            continue;
                        };
                        let sliced = {
                            if index >= fanout.slots() {
                                continue;
                            }
                            let sliced = fanout.process(
                                index,
                                &buf[..result.samples_read],
                                result.time_ticks,
                            );
                            (sliced.time_ticks, sliced.samples.to_vec())
                        };
                        let (slice_ticks, samples) = sliced;
                        let rate = self.slot_rates.get(index).copied().unwrap_or(0.0);
                        let seq = seqs.entry(session.session_id).or_insert(0);
                        let mut guard = session.peer.lock();
                        let Some(path) = guard.as_mut() else { continue };
                        let mut offset = 0usize;
                        while offset < samples.len() {
                            let end = (offset + self.samples_per_packet).min(samples.len());
                            let flags = if offset == 0 && result.overflow {
                                FLAG_RX_DISCONTINUITY
                            } else {
                                0
                            };
                            if let Err(error) = encode_packet(
                                &PacketHeader {
                                    stream: StreamKind::RxSamples,
                                    flags,
                                    session_id: session.session_id,
                                    seq: *seq,
                                    time_ticks: slice_ticks.saturating_add(
                                        crate::network::wire::samples_to_ticks(
                                            offset as u64,
                                            self.tick_rate,
                                            rate,
                                        ),
                                    ),
                                    sample_count: (end - offset) as u16,
                                    block_id: 0,
                                    block_offset: 0,
                                    block_sample_count: 0,
                                },
                                &samples[offset..end],
                                &mut scratch,
                            ) {
                                warn!("radiod: cannot encode packet: {error}");
                                break;
                            }
                            *seq = seq.wrapping_add(1);
                            if let Err(e) = path.send(&socket, &scratch) {
                                debug!("radiod: rx send failed: {e}");
                                break;
                            }
                            delivered = true;
                            offset = end;
                        }
                    }
                    continue;
                }
                let Some(session) = open.first() else {
                    continue;
                };
                let mut guard = session.peer.lock();
                let Some(path) = guard.as_mut() else { continue };
                let rate = f64::from_bits(self.rx_sample_rate_hz.load(Ordering::Relaxed));
                let read_ticks = result.time_ticks;
                let mut offset = 0usize;
                while offset < result.samples_read {
                    let end = (offset + self.samples_per_packet).min(result.samples_read);
                    let flags = if offset == 0 && result.overflow {
                        FLAG_RX_DISCONTINUITY
                    } else {
                        0
                    };
                    if let Err(error) = encode_packet(
                        &PacketHeader {
                            stream: StreamKind::RxSamples,
                            flags,
                            session_id: session.session_id,
                            seq,
                            time_ticks: read_ticks.saturating_add(
                                crate::network::wire::samples_to_ticks(
                                    offset as u64,
                                    self.tick_rate,
                                    rate,
                                ),
                            ),
                            sample_count: (end - offset) as u16,
                            block_id: 0,
                            block_offset: 0,
                            block_sample_count: 0,
                        },
                        &buf[offset..end],
                        &mut scratch,
                    ) {
                        warn!("radiod: cannot encode packet: {error}");
                        break;
                    }
                    seq = seq.wrapping_add(1);
                    if let Err(e) = path.send(&socket, &scratch) {
                        debug!("radiod: rx send failed: {e}");
                        break;
                    }
                    delivered = true;
                    offset = end;
                }
            } else {
                match cmds.recv_timeout(IDLE_COMMAND_POLL) {
                    Ok(cmd) => {
                        if self.handle_command(cmd, &mut rx, &mut active) {
                            return;
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                }
            }
        }
    }

    fn handle_command(
        &mut self,
        cmd: RxCommand,
        rx: &mut Box<dyn RadioRx>,
        active: &mut bool,
    ) -> bool {
        match cmd {
            RxCommand::RetireSlot { slot, reply } => {
                if let Some(fanout) = self.fanout.as_mut()
                    && fanout.retire_slot(slot)
                {
                    info!("radiod: composite RX slot {slot} released for reuse");
                }
                let _ = reply.send(Ok(()));
                false
            }
            RxCommand::RetuneSlot { slot, plan, reply } => {
                let device_rate_hz = self.device_rate_hz;
                let result = match &mut self.fanout {
                    Some(fanout) => fanout.retune_slot(slot, &plan, device_rate_hz).map(|()| {
                        info!(
                            "radiod: composite RX slot {slot} retuned to {:+} Hz from center",
                            plan.offset_hz
                        );
                    }),
                    None => Err(Error::from(
                        "this daemon serves one client at a time, so it has no composite slots",
                    )),
                };
                let _ = reply.send(result);
                false
            }
            RxCommand::AddSlot {
                plan,
                sample_rate_hz,
                reply,
            } => {
                let device_rate_hz = self.device_rate_hz;
                let result = match &mut self.fanout {
                    Some(fanout) => {
                        let index = fanout.add_slot(&plan, device_rate_hz);
                        while self.slot_rates.len() <= index {
                            self.slot_rates.push(sample_rate_hz);
                        }
                        self.slot_rates[index] = sample_rate_hz;
                        info!(
                            "radiod: composite RX slot {index} at {:+} Hz, {}/{} device samples per client sample",
                            plan.offset_hz, plan.ratio, plan.denominator
                        );
                        Ok(index)
                    }
                    None => Err(Error::from(
                        "this daemon serves one client at a time, so it has no composite slots",
                    )),
                };
                let _ = reply.send(result);
                false
            }
            RxCommand::Activate { time_ticks, reply } => {
                let result = rx.rx_activate(time_ticks);
                if result.is_ok() {
                    *active = true;
                }
                let _ = reply.send(result);
                false
            }
            RxCommand::SetFrequency {
                frequency_hz,
                reply,
            } => {
                let result = rx.set_rx_frequency(frequency_hz);
                let _ = reply.send(result);
                false
            }
            RxCommand::SetGain { gain_db, reply } => {
                let result = rx.set_rx_gain(gain_db).map(|()| rx.rx_reference_dbm());
                let _ = reply.send(result);
                false
            }
            RxCommand::Stop { reply } => {
                let result = rx.stop();
                *active = false;
                let _ = reply.send(result);
                false
            }
            RxCommand::Shutdown { reply } => {
                let _ = reply.send(rx.stop());
                *active = false;
                true
            }
        }
    }
}

#[cfg(test)]
mod tests;
