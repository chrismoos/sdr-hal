use super::*;

fn packet(seq: u32, time_ticks: u64, len: usize, discontinuity: bool) -> RxPacket {
    RxPacket {
        seq,
        time_ticks,
        discontinuity,
        samples: vec![Complex32::new(seq as f32, 0.0); len],
    }
}

fn test_rx(packets: Receiver<RxPacket>) -> NetworkRadioRx {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let channel = {
        let _guard = runtime.enter();
        Endpoint::from_static("http://127.0.0.1:1").connect_lazy()
    };
    NetworkRadioRx {
        rx_reference_dbm: None,
        rt: Arc::new(RpcRuntime {
            runtime: Some(runtime),
        }),
        client: RadioControlServiceClient::new(channel),
        clock: Arc::new(NetworkClock::new(1_000_000_000, 0)),
        packets,
        pending: None,
        expected_seq: None,
        minimum_time_ticks: None,
        pending_discontinuity: false,
        queue_gap: Arc::new(AtomicBool::new(false)),
        rx_sample_rate_hz: 1_000_000.0,
    }
}

#[test]
fn rx_read_assembles_contiguous_packets() {
    let (tx, rx) = crossbeam_channel::bounded(16);
    tx.send(packet(0, 1_000_000, 100, false)).unwrap();
    tx.send(packet(1, 1_100_000, 100, false)).unwrap();
    let mut radio_rx = test_rx(rx);

    let mut buf = vec![Complex32::default(); 150];
    let result = radio_rx.rx_read(&mut buf, 10_000).unwrap();
    assert_eq!(result.samples_read, 150);
    assert_eq!(result.time_ticks, 1_000_000);
    assert!(!result.overflow);
    assert_eq!(buf[0].re, 0.0);
    assert_eq!(buf[149].re, 1.0);

    let result = radio_rx.rx_read(&mut buf, 10_000).unwrap();
    assert_eq!(result.samples_read, 50);
    assert_eq!(result.time_ticks, 1_100_000 + 50_000);
    assert!(!result.overflow);
}

#[test]
fn rx_read_ends_at_seq_gap_and_flags_next_read() {
    let (tx, rx) = crossbeam_channel::bounded(16);
    tx.send(packet(0, 1_000_000, 100, false)).unwrap();
    tx.send(packet(5, 9_000_000, 100, false)).unwrap();
    let mut radio_rx = test_rx(rx);

    let mut buf = vec![Complex32::default(); 400];
    let result = radio_rx.rx_read(&mut buf, 10_000).unwrap();
    assert_eq!(result.samples_read, 100);
    assert!(!result.overflow);

    let result = radio_rx.rx_read(&mut buf, 10_000).unwrap();
    assert_eq!(result.samples_read, 100);
    assert_eq!(result.time_ticks, 9_000_000);
    assert!(result.overflow);
}

#[test]
fn rx_read_times_out_empty() {
    let (_tx, rx) = crossbeam_channel::bounded::<RxPacket>(16);
    let mut radio_rx = test_rx(rx);
    let mut buf = vec![Complex32::default(); 64];
    let started = Instant::now();
    let result = radio_rx.rx_read(&mut buf, 20_000).unwrap();
    assert_eq!(result.samples_read, 0);
    assert!(started.elapsed() >= Duration::from_millis(19));
}

#[test]
fn rx_read_reports_receiver_queue_drops() {
    let (tx, rx) = crossbeam_channel::bounded(16);
    tx.send(packet(0, 1_000_000, 10, false)).unwrap();
    let mut radio_rx = test_rx(rx);
    radio_rx.queue_gap.store(true, Ordering::Relaxed);
    let mut buf = vec![Complex32::default(); 10];
    let result = radio_rx.rx_read(&mut buf, 10_000).unwrap();
    assert!(result.overflow);
}

#[test]
fn rx_read_discards_packets_queued_before_retune() {
    let (tx, rx) = crossbeam_channel::bounded(16);
    tx.send(packet(0, 1_000_000, 100, false)).unwrap();
    tx.send(packet(1, 1_100_000, 100, false)).unwrap();
    tx.send(packet(2, 1_200_000, 100, false)).unwrap();
    let mut radio_rx = test_rx(rx);
    radio_rx.minimum_time_ticks = Some(1_150_000);
    radio_rx.pending_discontinuity = true;
    let mut buf = vec![Complex32::default(); 150];
    let result = radio_rx.rx_read(&mut buf, 0).unwrap();
    assert_eq!(result.samples_read, 150);
    assert_eq!(result.time_ticks, 1_150_000);
    assert!(result.overflow);
    assert_eq!(buf[0].re, 1.0);
    assert_eq!(buf[50].re, 2.0);
}

#[test]
fn fractional_rx_rate_is_used_for_partial_packet_timestamps() {
    let (tx, rx) = crossbeam_channel::bounded(2);
    tx.send(packet(0, 1_000_000, 10, false)).unwrap();
    let mut radio = test_rx(rx);
    radio.rx_sample_rate_hz = 2.5;
    let mut buf = [Complex32::default(); 3];
    assert_eq!(radio.rx_read(&mut buf, 0).unwrap().time_ticks, 1_000_000);
    assert_eq!(
        radio.rx_read(&mut buf, 0).unwrap().time_ticks,
        1_201_000_000
    );
}

#[test]
fn end_burst_preserves_stream_and_next_scheduled_block() {
    let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
    receiver
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.connect(receiver.local_addr().unwrap()).unwrap();
    let (_sender, packets) = crossbeam_channel::bounded(1);
    let rx = test_rx(packets);
    let mut tx = NetworkRadioTx {
        rt: rx.rt.clone(),
        client: rx.client.clone(),
        clock: rx.clock.clone(),
        socket,
        session_id: 7,
        seq: 0,
        block_id: 0,
        samples_per_packet: 350,
        tx_sample_rate_hz: 1_000_000.0,
        capabilities: [
            crate::TxCapability::BurstEnd,
            crate::TxCapability::TimedSamples,
        ]
        .into_iter()
        .collect(),
        active: true,
        calibration: Default::default(),
        scratch: Vec::new(),
        pace_next: Instant::now(),
        tx_stream: None,
        shutdown: Arc::new(AtomicBool::new(false)),
    };
    tx.transmit_at(&[Complex32::new(0.25, 0.0)], Some(100))
        .unwrap();
    tx.end_burst().unwrap();
    tx.transmit_at(&[Complex32::new(0.5, 0.0)], Some(200))
        .unwrap();
    for (seq, tick, boundary) in [(0, 100, false), (1, 0, true), (2, 200, false)] {
        let mut packet = [0u8; 256];
        let length = receiver.recv(&mut packet).unwrap();
        let header = decode_header(&packet[..length]).unwrap();
        assert_eq!(header.seq, seq);
        assert_eq!(header.block_id, seq);
        assert_eq!(header.time_ticks, tick);
        assert_eq!(header.sample_count, 1);
        assert_eq!(header.flags & wire::FLAG_TX_BURST_END != 0, boundary);
        assert_eq!(header.flags & FLAG_TX_NO_TICK != 0, boundary);
        assert_eq!(header.flags & FLAG_TX_DISABLE, 0);
        if boundary {
            assert!(
                packet[wire::HEADER_LEN..length]
                    .iter()
                    .all(|byte| *byte == 0)
            );
        }
    }
    assert!(tx.active);
    tx.capabilities = crate::TxCapabilities::default();
    assert!(tx.end_burst().is_err());
    tx.active = false;
}
