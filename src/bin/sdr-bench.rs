use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::Parser;
use num_complex::Complex32;
use sdr_hal::config::RadioConfig;
use sdr_hal::error::Error;

const TEST_SAMPLE_AMPLITUDE: f32 = 0.1;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    radio_config: PathBuf,
    #[arg(long)]
    frequency_hz: usize,
    #[arg(long)]
    sample_rate_hz: usize,
    #[arg(long)]
    bandwidth_hz: usize,
    #[arg(long)]
    batch_samples: usize,
    #[arg(long)]
    duration_seconds: u64,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    env_logger::init();
    let args = Args::parse();
    if args.sample_rate_hz == 0 || args.batch_samples == 0 || args.duration_seconds == 0 {
        return Err("sample rate, batch size and duration must be positive".into());
    }
    let mut config = RadioConfig::load(&args.radio_config)?;
    config.tx_sample_rate_hz = args.sample_rate_hz;
    config.tx_bandwidth_hz = Some(args.bandwidth_hz);
    config.rx_sample_rate_hz = None;
    config.rx_bandwidth_hz = None;
    let radio = config.open()?;
    let actual_sample_rate_hz = radio.tx_sample_rate_hz();
    let (mut tx, _) = radio.split()?;
    tx.set_tx_frequency_hz(args.frequency_hz as f64)?;
    tx.prepare_transmit(args.batch_samples)?;
    let samples = vec![Complex32::new(TEST_SAMPLE_AMPLITUDE, 0.0); args.batch_samples];
    let start = Instant::now();
    let deadline = start + Duration::from_secs(args.duration_seconds);
    let mut total_samples = 0_u64;
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    let result: Result<(), Error> = async {
        tx.enable_transmit(true)?;
        while Instant::now() < deadline {
            tokio::select! {
                signal = &mut shutdown => { signal?; break; }
                _ = tokio::task::yield_now() => {}
            }
            tx.transmit(&samples)?;
            total_samples += samples.len() as u64;
            let target =
                start + Duration::from_secs_f64(total_samples as f64 / actual_sample_rate_hz);
            let wait = target
                .saturating_duration_since(Instant::now())
                .min(deadline.saturating_duration_since(Instant::now()));
            tokio::select! {
                signal = &mut shutdown => { signal?; break; }
                _ = tokio::time::sleep(wait) => {}
            }
        }
        Ok(())
    }
    .await;
    let stopped = tx.stop();
    result?;
    stopped?;
    let health = tx.tx_health()?;
    println!(
        "sample_rate_hz={actual_sample_rate_hz} samples={total_samples} elapsed_seconds={:.3} underflows={} late_packets={} dropped_packets={}",
        start.elapsed().as_secs_f64(),
        health.underflows,
        health.late_packets,
        health.dropped_packets
    );
    Ok(())
}
