use num_complex::Complex32;
use sdr_hal::Radio;
use sdr_hal::memory::{IqBlock, MemoryRadio};

fn main() -> Result<(), sdr_hal::error::Error> {
    const SAMPLE_RATE_HZ: f64 = 2_000_000.0;
    const START_TIME_NS: u64 = 10_000;
    const BLOCK_SAMPLES: usize = 16;
    let (mut radio, handle) = MemoryRadio::new(2);
    let actual_rate_hz = radio.setup_rx(0, "", SAMPLE_RATE_HZ, SAMPLE_RATE_HZ, None)?;
    let (mut tx, rx) = Box::new(radio).split()?;
    let mut rx = rx.ok_or("memory radio has no RX")?;
    let samples = vec![Complex32::new(0.25, -0.5); BLOCK_SAMPLES];
    tx.transmit_at(&samples, Some(START_TIME_NS))?;
    let transmitted = handle.output.recv()?;
    handle.input.send(IqBlock {
        samples: transmitted.samples,
        time_ticks: transmitted.time_ticks,
        discontinuity: false,
    })?;
    rx.rx_activate(None)?;
    let mut output = vec![Complex32::default(); BLOCK_SAMPLES];
    let result = rx.rx_read(&mut output, 0)?;
    assert_eq!(output, samples);
    assert_eq!(result.time_ticks, START_TIME_NS);
    println!(
        "sample_rate_hz={actual_rate_hz} samples={} first_sample_time_ns={}",
        result.samples_read, result.time_ticks
    );
    Ok(())
}
