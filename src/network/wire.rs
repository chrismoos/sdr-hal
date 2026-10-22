//! UDP data-plane framing: fixed 36-byte big-endian header followed by
//! `sample_count` complex samples packed as Q15 i16 I/Q pairs.

use crate::error::Error;
use num_complex::Complex32;

/// Version byte accepted by this codec. Peers must use the same version.
pub const WIRE_VERSION: u8 = 4;
/// Fixed header length in bytes, including two reserved zero bytes at offsets 22..24.
pub const HEADER_LEN: usize = 36;
/// Bytes per complex sample: two big-endian signed 16-bit components, I then Q.
pub const BYTES_PER_SAMPLE: usize = 4;

/// Default packet capacity, fitting a 1472-byte UDP payload including the header.
pub const DEFAULT_SAMPLES_PER_PACKET: usize = 350;
/// Maximum sample count negotiated by the daemon. Larger UDP packets can require fragmentation.
pub const MAX_SAMPLES_PER_PACKET: usize = 2200;

const Q15_SCALE: f32 = 32767.0;

/// IQ transport, serialized as lowercase `udp` or `tcp`. It applies to both
/// directions, so RX and the clock heartbeat use whichever one TX uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// Datagrams with sequence numbers and block assembly. The default transport.
    #[default]
    Udp,
    /// Ordered byte stream of the same header-and-payload packets, with lengths derived from the header.
    Tcp,
}

/// The receive stream saw a discontinuity (packet loss or hardware
/// overflow) immediately before this packet's first sample.
pub const FLAG_RX_DISCONTINUITY: u16 = 1 << 0;
/// Ignores `time_ticks` for this TX block and submits without a timestamp.
pub const FLAG_TX_NO_TICK: u16 = 1 << 1;
/// Marks the first fragment of a TX block. A large TX call can contain several blocks.
pub const FLAG_TX_BLOCK_START: u16 = 1 << 2;
/// Marks the last fragment of a TX block. Missing fragments may be replaced by silence.
pub const FLAG_TX_BLOCK_END: u16 = 1 << 3;
/// TX disable fence ordered after preceding data packets. Used with an empty TX payload.
pub const FLAG_TX_DISABLE: u16 = 1 << 4;
/// Orders TX enable after earlier TCP stop fences and before subsequent IQ.
pub const FLAG_TX_ENABLE: u16 = 1 << 6;

/// Marks the final fragment of the final TX block in a burst. It does not stop the stream.
pub const FLAG_TX_BURST_END: u16 = 1 << 5;

/// Wire message discriminator. Unknown numeric values are rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    /// Establishes the return path for a session on the transport it arrives over.
    Hello = 1,
    /// Client-to-server IQ fragment or ordered TX control fence.
    TxSamples = 2,
    /// Server-to-client IQ samples.
    RxSamples = 3,
    /// Server clock observation in `time_ticks`, without IQ samples.
    ClockHeartbeat = 4,
}

impl StreamKind {
    fn from_wire(value: u8) -> Result<Self, Error> {
        match value {
            1 => Ok(Self::Hello),
            2 => Ok(Self::TxSamples),
            3 => Ok(Self::RxSamples),
            4 => Ok(Self::ClockHeartbeat),
            other => Err(Error::new(
                crate::error::ErrorKind::Protocol,
                format!("network radio: unknown stream kind {other}"),
            )),
        }
    }
}

/// Decoded version 3 header. Block offsets and lengths count complex samples, not bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketHeader {
    /// Message discriminator at byte offset 1.
    pub stream: StreamKind,
    /// Bitwise combination of the `FLAG_*` constants.
    pub flags: u16,
    /// Server-issued session token. Packets from other sessions must be discarded.
    pub session_id: u32,
    /// Wrapping packet sequence number for transport loss and ordering checks.
    pub seq: u32,
    /// First-sample or heartbeat time in device ticks. Ignored for untimed TX.
    pub time_ticks: u64,
    /// Complex samples in this packet payload.
    pub sample_count: u16,
    /// Wrapping TX-block identifier shared by its fragments. Zero for RX and heartbeat packets.
    pub block_id: u32,
    /// Complex-sample offset of this fragment within the TX block.
    pub block_offset: u32,
    /// Total complex samples in the TX block, shared by all its fragments. Zero for non-TX packets.
    pub block_sample_count: u32,
}

/// Replaces `out` with the header and quantized IQ payload. Components are clamped to
/// [-1, 1], multiplied by 32767, and rounded. `header.sample_count` must equal
/// `samples.len()` and fit in `u16`. Invalid lengths return a protocol error in every build.
pub fn encode_packet(
    header: &PacketHeader,
    samples: &[Complex32],
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    if samples.len() != usize::from(header.sample_count) {
        return Err(Error::new(
            crate::error::ErrorKind::Protocol,
            "packet sample count does not match payload",
        ));
    }
    out.clear();
    out.reserve(HEADER_LEN + samples.len() * BYTES_PER_SAMPLE);
    out.push(WIRE_VERSION);
    out.push(header.stream as u8);
    out.extend_from_slice(&header.flags.to_be_bytes());
    out.extend_from_slice(&header.session_id.to_be_bytes());
    out.extend_from_slice(&header.seq.to_be_bytes());
    out.extend_from_slice(&header.time_ticks.to_be_bytes());
    out.extend_from_slice(&header.sample_count.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&header.block_id.to_be_bytes());
    out.extend_from_slice(&header.block_offset.to_be_bytes());
    out.extend_from_slice(&header.block_sample_count.to_be_bytes());
    for sample in samples {
        out.extend_from_slice(&f32_to_q15(sample.re).to_be_bytes());
        out.extend_from_slice(&f32_to_q15(sample.im).to_be_bytes());
    }
    Ok(())
}

/// Checks the packet version, stream kind, header, and payload length. Trailing
/// bytes are allowed. You must still validate the session token, flags, and block ranges.
pub fn decode_header(buf: &[u8]) -> Result<PacketHeader, Error> {
    if buf.len() < HEADER_LEN {
        return Err(Error::new(
            crate::error::ErrorKind::Protocol,
            format!(
                "network radio: packet shorter than header: {} < {HEADER_LEN}",
                buf.len()
            ),
        ));
    }
    if buf[0] != WIRE_VERSION {
        return Err(Error::new(
            crate::error::ErrorKind::Protocol,
            format!("network radio: unsupported wire version {}", buf[0]),
        ));
    }
    let header = PacketHeader {
        stream: StreamKind::from_wire(buf[1])?,
        flags: u16::from_be_bytes(buf[2..4].try_into().unwrap()),
        session_id: u32::from_be_bytes(buf[4..8].try_into().unwrap()),
        seq: u32::from_be_bytes(buf[8..12].try_into().unwrap()),
        time_ticks: u64::from_be_bytes(buf[12..20].try_into().unwrap()),
        sample_count: u16::from_be_bytes(buf[20..22].try_into().unwrap()),
        block_id: u32::from_be_bytes(buf[24..28].try_into().unwrap()),
        block_offset: u32::from_be_bytes(buf[28..32].try_into().unwrap()),
        block_sample_count: u32::from_be_bytes(buf[32..36].try_into().unwrap()),
    };
    let expected = HEADER_LEN + header.sample_count as usize * BYTES_PER_SAMPLE;
    if buf.len() < expected {
        return Err(Error::new(
            crate::error::ErrorKind::Protocol,
            format!(
                "network radio: truncated payload: {} < {expected}",
                buf.len()
            ),
        ));
    }
    Ok(header)
}

/// Reads the advertised payload byte count after validating header length and version.
/// Does not require the payload to be present or validate the stream discriminator.
pub fn payload_len(buf: &[u8]) -> Result<usize, Error> {
    if buf.len() < HEADER_LEN {
        return Err(Error::new(
            crate::error::ErrorKind::Protocol,
            format!(
                "network radio: packet shorter than header: {} < {HEADER_LEN}",
                buf.len()
            ),
        ));
    }
    if buf[0] != WIRE_VERSION {
        return Err(Error::new(
            crate::error::ErrorKind::Protocol,
            format!("network radio: unsupported wire version {}", buf[0]),
        ));
    }
    let sample_count = u16::from_be_bytes(buf[20..22].try_into().unwrap()) as usize;
    Ok(sample_count * BYTES_PER_SAMPLE)
}

/// Replaces `out` with decoded IQ, dividing signed components by 32767.
///
/// Rejects truncated payloads and a header that does not match the packet.
pub fn decode_samples_into(
    buf: &[u8],
    header: &PacketHeader,
    out: &mut Vec<Complex32>,
) -> Result<(), Error> {
    let decoded = decode_header(buf)?;
    if decoded != *header {
        return Err(Error::new(
            crate::error::ErrorKind::Protocol,
            "header does not belong to this packet",
        ));
    }
    out.clear();
    out.reserve(header.sample_count as usize);
    let payload = &buf[HEADER_LEN..HEADER_LEN + header.sample_count as usize * BYTES_PER_SAMPLE];
    for pair in payload.chunks_exact(BYTES_PER_SAMPLE) {
        let re = i16::from_be_bytes(pair[0..2].try_into().unwrap());
        let im = i16::from_be_bytes(pair[2..4].try_into().unwrap());
        out.push(Complex32::new(q15_to_f32(re), q15_to_f32(im)));
    }
    Ok(())
}

fn f32_to_q15(value: f32) -> i16 {
    (value.clamp(-1.0, 1.0) * Q15_SCALE).round() as i16
}

fn q15_to_f32(value: i16) -> f32 {
    value as f32 / Q15_SCALE
}

/// Converts a sample count to device ticks. Exact whenever the tick rate is a
/// whole multiple of the sample rate, which is the usual case. Zero rate returns zero.
#[cfg(feature = "network-backend")]
pub(crate) fn samples_to_ticks(samples: u64, tick_rate: u64, sample_rate_hz: f64) -> u64 {
    if sample_rate_hz <= 0.0 {
        return 0;
    }
    (samples as f64 * tick_rate as f64 / sample_rate_hz).floor() as u64
}

/// Converts a sample count to wall-clock nanoseconds, for pacing rather than device time.
#[cfg(feature = "network-backend")]
pub(crate) fn samples_to_ns_exact(samples: u64, rate_hz: f64) -> u64 {
    (samples as f64 * 1_000_000_000.0 / rate_hz).floor() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(sample_count: u16) -> PacketHeader {
        PacketHeader {
            stream: StreamKind::TxSamples,
            flags: FLAG_RX_DISCONTINUITY,
            session_id: 0xA1B2C3D4,
            seq: 42,
            time_ticks: 123_456_789_000,
            sample_count,
            block_id: 7,
            block_offset: 350,
            block_sample_count: 700,
        }
    }

    #[test]
    fn packet_round_trip_preserves_header_and_samples() {
        let samples: Vec<Complex32> = (0..350)
            .map(|i| Complex32::new((i as f32 / 350.0) - 0.5, 0.5 - (i as f32 / 350.0)))
            .collect();
        let hdr = header(samples.len() as u16);
        let mut buf = Vec::new();
        encode_packet(&hdr, &samples, &mut buf).unwrap();
        assert_eq!(buf.len(), HEADER_LEN + samples.len() * BYTES_PER_SAMPLE);

        let decoded_hdr = decode_header(&buf).expect("decode header");
        assert_eq!(decoded_hdr, hdr);
        let mut decoded = Vec::new();
        decode_samples_into(&buf, &decoded_hdr, &mut decoded).unwrap();
        assert_eq!(decoded.len(), samples.len());
        for (a, b) in samples.iter().zip(decoded.iter()) {
            assert!((a.re - b.re).abs() < 1.0 / Q15_SCALE);
            assert!((a.im - b.im).abs() < 1.0 / Q15_SCALE);
        }
    }

    #[test]
    fn empty_tx_control_packet_preserves_disable_flag() {
        let mut hdr = header(0);
        hdr.flags = FLAG_TX_DISABLE;
        hdr.block_id = 0;
        hdr.block_offset = 0;
        hdr.block_sample_count = 0;
        let mut buf = Vec::new();
        encode_packet(&hdr, &[], &mut buf).unwrap();

        assert_eq!(buf.len(), HEADER_LEN);
        assert_eq!(decode_header(&buf).expect("decode header"), hdr);
    }

    #[test]
    fn q15_clamps_out_of_range_values() {
        assert_eq!(f32_to_q15(2.0), i16::MAX);
        assert_eq!(f32_to_q15(-2.0), -i16::MAX);
    }

    #[test]
    fn default_packet_fits_standard_mtu() {
        const MAX_UDP_PAYLOAD_STANDARD_MTU: usize = 1472;
        const {
            assert!(
                HEADER_LEN + DEFAULT_SAMPLES_PER_PACKET * BYTES_PER_SAMPLE
                    <= MAX_UDP_PAYLOAD_STANDARD_MTU
            );
        }
    }

    #[test]
    fn decode_rejects_bad_version_and_truncation() {
        let samples = vec![Complex32::new(0.1, -0.1); 8];
        let hdr = header(samples.len() as u16);
        let mut buf = Vec::new();
        encode_packet(&hdr, &samples, &mut buf).unwrap();

        let mut bad_version = buf.clone();
        bad_version[0] = 9;
        assert!(decode_header(&bad_version).is_err());

        assert!(decode_header(&buf[..HEADER_LEN - 1]).is_err());
        assert!(decode_header(&buf[..buf.len() - 1]).is_err());
    }

    #[test]
    #[cfg(feature = "network-backend")]
    fn sample_to_tick_conversion_is_exact_on_whole_multiples() {
        // A UHD master clock that is 20 times the sample rate.
        assert_eq!(samples_to_ticks(350, 39_321_600, 1_966_080.0), 7_000);
        // Lime and bladeRF clock in samples, so the count passes through.
        assert_eq!(samples_to_ticks(12_288, 9_830_400, 9_830_400.0), 12_288);
        assert_eq!(samples_to_ticks(0, 39_321_600, 1_966_080.0), 0);
        assert_eq!(samples_to_ticks(5, 39_321_600, 0.0), 0);
    }

    #[test]
    fn malformed_payloads_fail_without_changing_output() {
        let samples = [Complex32::default(); 2];
        let mut encoded = vec![99];
        assert!(encode_packet(&header(1), &samples, &mut encoded).is_err());
        assert_eq!(encoded, vec![99]);
        let hdr = header(2);
        encode_packet(&hdr, &samples, &mut encoded).unwrap();
        let mut decoded = vec![Complex32::new(1.0, 1.0)];
        assert!(decode_samples_into(&encoded[..encoded.len() - 1], &hdr, &mut decoded).is_err());
        assert!(decode_samples_into(&encoded, &header(1), &mut decoded).is_err());
        assert_eq!(decoded, vec![Complex32::new(1.0, 1.0)]);
    }
}
