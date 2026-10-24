use super::*;
use crate::network::wire::{FLAG_TX_BLOCK_END, FLAG_TX_BLOCK_START};

const RATE: u64 = 4_915_200;
// The device clocks in samples, so a sample offset is a tick offset.
const TICK_RATE: u64 = RATE;

fn tx_header(
    block_id: u32,
    offset: u32,
    total: u32,
    flags: u16,
    start_ticks: u64,
    count: usize,
) -> PacketHeader {
    PacketHeader {
        stream: StreamKind::TxSamples,
        flags,
        session_id: 1,
        seq: offset,
        time_ticks: start_ticks + offset as u64,
        sample_count: count as u16,
        block_id,
        block_offset: offset,
        block_sample_count: total,
    }
}

#[test]
fn assembler_reorders_packets_and_submits_one_complete_block() {
    let now = Instant::now();
    let start_ticks = 10_000_000;
    let mut assembler = TxBlockAssembler::default();
    let tail = [Complex32::new(3.0, 0.0), Complex32::new(4.0, 0.0)];
    let head = [Complex32::new(1.0, 0.0), Complex32::new(2.0, 0.0)];

    assert!(
        assembler
            .push(
                &tx_header(7, 2, 4, FLAG_TX_BLOCK_END, start_ticks, tail.len()),
                &tail,
                TICK_RATE,
                RATE as f64,
                now,
            )
            .unwrap()
            .is_empty()
    );
    let completed = assembler
        .push(
            &tx_header(7, 0, 4, FLAG_TX_BLOCK_START, start_ticks, head.len()),
            &head,
            TICK_RATE,
            RATE as f64,
            now,
        )
        .unwrap();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].missing_samples, 0);
    assert_eq!(completed[0].time_ticks, Some(start_ticks));
    assert_eq!(completed[0].samples, [head, tail].concat());
}

#[test]
fn assembler_ignores_duplicates_and_zero_fills_loss_after_grace() {
    let now = Instant::now();
    let start_ticks = 20_000_000;
    let mut assembler = TxBlockAssembler::default();
    let head = [Complex32::new(1.0, 0.0), Complex32::new(2.0, 0.0)];
    let header = tx_header(
        8,
        0,
        4,
        FLAG_TX_BLOCK_START | FLAG_TX_BLOCK_END,
        start_ticks,
        head.len(),
    );
    assert!(
        assembler
            .push(&header, &head, TICK_RATE, RATE as f64, now)
            .unwrap()
            .is_empty()
    );
    assert!(
        assembler
            .push(&header, &head, TICK_RATE, RATE as f64, now)
            .unwrap()
            .is_empty()
    );
    let block = assembler
        .expire(now + TX_BLOCK_REORDER_GRACE)
        .expect("incomplete block expires");
    assert_eq!(block.missing_samples, 2);
    assert_eq!(&block.samples[..2], &head);
    assert_eq!(&block.samples[2..], &[Complex32::default(); 2]);
}

#[test]
fn assembler_ignores_late_block_without_displacing_pending_block() {
    let now = Instant::now();
    let mut assembler = TxBlockAssembler::default();
    let sample = [Complex32::new(9.0, 0.0)];

    assembler
        .push(
            &tx_header(10, 0, 2, FLAG_TX_BLOCK_START, 30_000_000, 1),
            &sample,
            TICK_RATE,
            RATE as f64,
            now,
        )
        .unwrap();
    let completed = assembler
        .push(
            &tx_header(
                9,
                0,
                1,
                FLAG_TX_BLOCK_START | FLAG_TX_BLOCK_END,
                20_000_000,
                1,
            ),
            &sample,
            TICK_RATE,
            RATE as f64,
            now,
        )
        .unwrap();
    assert!(completed.is_empty());

    let tail = [Complex32::new(10.0, 0.0)];
    let completed = assembler
        .push(
            &tx_header(10, 1, 2, FLAG_TX_BLOCK_END, 30_000_000, 1),
            &tail,
            TICK_RATE,
            RATE as f64,
            now,
        )
        .unwrap();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].block_id, 10);
    assert_eq!(completed[0].samples, [sample, tail].concat());
}

#[test]
fn assembler_rejects_fragment_outside_declared_block() {
    let mut assembler = TxBlockAssembler::default();
    let samples = [Complex32::new(1.0, 0.0); 2];
    let error = assembler
        .push(
            &tx_header(11, 3, 4, FLAG_TX_BLOCK_END, 40_000_000, samples.len()),
            &samples,
            TICK_RATE,
            RATE as f64,
            Instant::now(),
        )
        .expect_err("range must be rejected");
    assert!(error.to_string().contains("exceeds total 4"), "{error}");
}

#[test]
fn assembler_drops_block_without_end_after_silence_timeout() {
    let now = Instant::now();
    let mut assembler = TxBlockAssembler::default();
    let head = [Complex32::new(1.0, 0.0); 2];
    assembler
        .push(
            &tx_header(12, 0, 4, FLAG_TX_BLOCK_START, 50_000_000, head.len()),
            &head,
            TICK_RATE,
            RATE as f64,
            now,
        )
        .unwrap();
    assert_eq!(assembler.drop_stalled(now + TX_BLOCK_REORDER_GRACE), None);
    assert_eq!(
        assembler.drop_stalled(now + TX_SILENCE_TIMEOUT),
        Some((12, 2))
    );
    assert_eq!(assembler.pending_shortfall(), None);
    assert!(assembler.is_late(12));
}

#[test]
fn assembler_discard_pending_clears_incomplete_block() {
    let mut assembler = TxBlockAssembler::default();
    let head = [Complex32::new(1.0, 0.0)];
    assembler
        .push(
            &tx_header(13, 0, 3, FLAG_TX_BLOCK_START, 60_000_000, head.len()),
            &head,
            TICK_RATE,
            RATE as f64,
            Instant::now(),
        )
        .unwrap();
    assert_eq!(assembler.discard_pending(), Some((13, 2)));
    assert_eq!(assembler.discard_pending(), None);
}

#[test]
fn silence_watchdog_fires_only_while_armed_and_quiet() {
    let start = Instant::now();
    let mut watchdog = TxSilenceWatchdog::default();
    assert_eq!(watchdog.expired(start + TX_SILENCE_TIMEOUT * 2), None);

    watchdog.arm(start);
    let half = TX_SILENCE_TIMEOUT / 2;
    assert_eq!(watchdog.expired(start + half), None);
    watchdog.note_samples(start + half);
    assert_eq!(watchdog.expired(start + TX_SILENCE_TIMEOUT), None);
    assert_eq!(
        watchdog.expired(start + half + TX_SILENCE_TIMEOUT),
        Some(TX_SILENCE_TIMEOUT)
    );

    watchdog.disarm();
    watchdog.note_samples(start);
    assert_eq!(watchdog.expired(start + TX_SILENCE_TIMEOUT * 4), None);
}

#[test]
fn a_continuous_packet_backlog_cannot_extend_the_silence_deadline_indefinitely() {
    let start = Instant::now();
    let mut wait = BacklogWait::default();
    assert!(wait.defer_missing(TX_PACKET_BATCH_LIMIT + 1, start));
    assert!(wait.defer_missing(TX_PACKET_BATCH_LIMIT, start + COMPOSITE_BACKLOG_GRACE / 2));
    assert!(!wait.defer_missing(TX_PACKET_QUEUE_CAPACITY, start + COMPOSITE_BACKLOG_GRACE));
    assert!(!wait.defer_missing(
        TX_PACKET_QUEUE_CAPACITY,
        start + COMPOSITE_BACKLOG_GRACE * 2
    ));
    assert!(!wait.defer_missing(0, start + COMPOSITE_BACKLOG_GRACE * 2));
    assert!(wait.defer_missing(1, start + COMPOSITE_BACKLOG_GRACE * 3));
}

#[test]
fn packets_queued_beyond_one_batch_fill_the_grid_before_silence_is_committed() {
    use crate::daemon::composite::{CompositeConfig, Direction, Staged};

    const DEVICE_RATE: u64 = 3_888_000;
    const CLIENT_RATE: f64 = 388_800.0;
    const CENTER: u64 = 867_520_000;
    const BLOCK_SAMPLES: usize = 3240;
    const BLOCK_INDEX: u64 = 10;
    const BACKLOG_PACKETS: usize = TX_PACKET_BATCH_LIMIT + 108;
    let config = CompositeConfig {
        device_rate_hz: DEVICE_RATE,
        tx_center_hz: CENTER,
        rx_center_hz: CENTER,
        tx_digital_backoff_db: 0.0,
        block_duration_ms: 5.0 / 6.0,
        tx_deadline_ms: 2.0,
    };
    let plan = config
        .plan_slice(Direction::Tx, CLIENT_RATE, CENTER, Some(30_000))
        .unwrap();
    let make_grid = || {
        let mut grid = TxGrid::new(DEVICE_RATE, DEVICE_RATE, BLOCK_SAMPLES).unwrap();
        grid.add_slot(&plan, DEVICE_RATE, 1.0);
        grid.add_slot(&plan, DEVICE_RATE, 1.0);
        grid
    };
    let mut grid = make_grid();
    let mut reference = make_grid();
    let samples = vec![Complex32::new(0.25, 0.0); BLOCK_SAMPLES / plan.ratio];
    let ticks = BLOCK_INDEX * grid.block_ticks();
    for slot in 0..2 {
        reference.stage(slot, ticks, &samples);
    }
    grid.stage(0, ticks, &samples);
    let now = Instant::now();
    let mut wait = BacklogWait::default();
    let queued = BACKLOG_PACKETS - TX_PACKET_BATCH_LIMIT;
    let deadline = (!wait.defer_missing(queued, now)).then_some(BLOCK_INDEX);
    assert!(
        grid.emit_available(BLOCK_INDEX, deadline, |_| true)
            .is_none()
    );
    assert!(matches!(
        grid.stage(1, ticks, &samples),
        Staged::Placed { .. }
    ));
    let deadline =
        (!wait.defer_missing(queued, now + COMPOSITE_BACKLOG_GRACE / 2)).then_some(BLOCK_INDEX);
    let actual = grid
        .emit_available(BLOCK_INDEX, deadline, |_| true)
        .unwrap();
    let expected = reference
        .emit_available(BLOCK_INDEX, Some(BLOCK_INDEX), |_| true)
        .unwrap();
    assert_eq!(actual.time_ticks, expected.time_ticks);
    assert_eq!(actual.samples, expected.samples);
}
