//! IQ exchange over bounded channels. TX timestamps are metadata, not wall-clock scheduling.
//!
//! ```
//! use num_complex::Complex32;
//! use sdr_hal::{Radio, memory::MemoryRadio};
//!
//! # fn main() -> Result<(), sdr_hal::error::Error> {
//! let (mut radio, peer) = MemoryRadio::new(1);
//! radio.setup_rx(0, "", 2_000_000.0, 2_000_000.0, None)?;
//! let (mut tx, rx) = Box::new(radio).split()?;
//! tx.transmit_at(&[Complex32::new(0.5, -0.25)], Some(10_000))?;
//! peer.input.send(peer.output.recv()?)?;
//! let mut rx = rx.expect("RX was configured");
//! rx.rx_activate(None)?;
//! let mut iq = [Complex32::default(); 1];
//! let read = rx.rx_read(&mut iq, 0)?;
//! assert_eq!(read.time_ticks, 10_000);
//! assert_eq!(iq[0], Complex32::new(0.5, -0.25));
//! rx.stop()?;
//! # Ok(())
//! # }
//! ```

use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::{Duration, Instant};

use num_complex::Complex32;

use crate::{Radio, RadioRx, RadioTx, RxReadResult, error::Error};

const TICK_RATE_HZ: u64 = 1_000_000_000;

/// An IQ block with an optional timestamp and a flag for lost samples.
pub struct IqBlock {
    /// Samples in time order. No interpolation or amplitude conversion is applied.
    pub samples: Vec<Complex32>,
    /// First-sample timestamp in nanoseconds. RX uses current elapsed time when absent.
    pub time_ticks: Option<u64>,
    /// Marks a gap before the first sample. RX reports it only on the first read of this block.
    pub discontinuity: bool,
}

/// In-process IQ backend. Its nanosecond clock starts at construction.
///
/// TX sends immediately to the bounded output channel, blocking when full.
/// Enable and disable are no-ops. RX activation ticks are ignored. Partial RX
/// reads preserve the block timestamp plus the consumed-sample duration.
pub struct MemoryRadio {
    output: SyncSender<IqBlock>,
    input: Receiver<IqBlock>,
    clock_start: Instant,
    rx_configured: bool,
    tx_sample_rate_hz: f64,
    rx_sample_rate_hz: f64,
}

/// The other end of the radio queues. Send blocks to RX and receive blocks from TX.
/// Dropping either endpoint disconnects that stream.
pub struct MemoryRadioHandle {
    /// Receives each TX call as one owned block. Drain it to prevent TX from blocking.
    pub output: Receiver<IqBlock>,
    /// Sends blocks to RX. A send waits if the queue is full.
    pub input: SyncSender<IqBlock>,
}

impl MemoryRadio {
    /// Creates paired RX and TX queues of `buffer_depth` blocks. Zero uses rendezvous channels.
    /// Call [`Radio::setup_rx`] before splitting to retain the RX endpoint.
    pub fn new(buffer_depth: usize) -> (Self, MemoryRadioHandle) {
        let (output, output_rx) = sync_channel(buffer_depth);
        let (input_tx, input) = sync_channel(buffer_depth);
        (
            Self {
                output,
                input,
                clock_start: Instant::now(),
                rx_configured: false,
                tx_sample_rate_hz: 0.0,
                rx_sample_rate_hz: 0.0,
            },
            MemoryRadioHandle {
                output: output_rx,
                input: input_tx,
            },
        )
    }
}

impl Radio for MemoryRadio {
    fn tx_capabilities(&self) -> crate::TxCapabilities {
        let capabilities = crate::TxCapabilities::from([crate::TxCapability::BurstEnd]);
        if self.supports_timed_tx() {
            capabilities.with(crate::TxCapability::TimedSamples)
        } else {
            capabilities
        }
    }

    fn tx_sample_rate_hz(&self) -> f64 {
        self.tx_sample_rate_hz
    }
    fn rx_sample_rate_hz(&self) -> Option<f64> {
        self.rx_configured.then_some(self.rx_sample_rate_hz)
    }
    fn supports_timed_tx(&self) -> bool {
        true
    }
    fn tick_rate(&self) -> u64 {
        TICK_RATE_HZ
    }
    fn set_tx_frequency(&mut self, _frequency_hz: usize) -> Result<(), Error> {
        Ok(())
    }
    fn set_tx_sample_rate(&mut self, sample_rate_hz: f64) -> Result<f64, Error> {
        crate::stream::positive_hz(sample_rate_hz, "set_tx_sample_rate")?;
        self.tx_sample_rate_hz = sample_rate_hz;
        Ok(sample_rate_hz)
    }
    fn set_tx_bandwidth(&mut self, _bandwidth_hz: usize) -> Result<(), Error> {
        Ok(())
    }
    fn setup_rx(
        &mut self,
        _channel: usize,
        _antenna: &str,
        sample_rate_hz: f64,
        _bandwidth_hz: f64,
        _gain_db: Option<f64>,
    ) -> Result<f64, Error> {
        if !sample_rate_hz.is_finite() || sample_rate_hz <= 0.0 {
            return Err("RX sample rate must be finite and positive".into());
        }
        self.rx_sample_rate_hz = sample_rate_hz;
        self.rx_configured = true;
        Ok(sample_rate_hz)
    }
    fn split(self: Box<Self>) -> Result<(Box<dyn RadioTx>, Option<Box<dyn RadioRx>>), Error> {
        let Self {
            output,
            input,
            clock_start,
            rx_configured,
            tx_sample_rate_hz,
            rx_sample_rate_hz,
        } = *self;
        let tx = MemoryTx {
            tx_sample_rate_hz,
            output,
            clock_start,
        };
        let rx = rx_configured.then(|| {
            Box::new(MemoryRx {
                input,
                clock_start,
                sample_rate_hz: rx_sample_rate_hz,
                pending: None,
                active: false,
            }) as Box<dyn RadioRx>
        });
        Ok((Box::new(tx), rx))
    }
}

struct MemoryTx {
    tx_sample_rate_hz: f64,
    output: SyncSender<IqBlock>,
    clock_start: Instant,
}

impl RadioTx for MemoryTx {
    fn capabilities(&self) -> crate::TxCapabilities {
        crate::TxCapabilities::from([
            crate::TxCapability::TimedSamples,
            crate::TxCapability::BurstEnd,
        ])
    }
    fn end_burst(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn tx_sample_rate_hz(&self) -> f64 {
        self.tx_sample_rate_hz
    }
    fn set_tx_frequency_hz(&mut self, _frequency_hz: f64) -> Result<(), Error> {
        Ok(())
    }

    fn supports_timed_tx(&self) -> bool {
        true
    }
    fn tick_rate(&self) -> u64 {
        TICK_RATE_HZ
    }
    fn get_hardware_time(&self) -> Result<u64, Error> {
        Ok(self.clock_start.elapsed().as_nanos() as u64)
    }
    fn transmit(&mut self, samples: &[Complex32]) -> Result<(), Error> {
        self.transmit_at(samples, None)
    }
    fn transmit_at(&mut self, samples: &[Complex32], time_ticks: Option<u64>) -> Result<(), Error> {
        self.output.send(IqBlock {
            samples: samples.to_vec(),
            time_ticks,
            discontinuity: false,
        })?;
        Ok(())
    }
    fn enable_transmit(&mut self, _enable: bool) -> Result<(), Error> {
        Ok(())
    }
}

struct MemoryRx {
    input: Receiver<IqBlock>,
    clock_start: Instant,
    sample_rate_hz: f64,
    pending: Option<(IqBlock, usize)>,
    active: bool,
}

impl RadioRx for MemoryRx {
    fn rx_sample_rate_hz(&self) -> f64 {
        self.sample_rate_hz
    }
    fn set_rx_frequency(&mut self, _frequency_hz: f64) -> Result<(), Error> {
        Ok(())
    }

    fn tick_rate(&self) -> u64 {
        TICK_RATE_HZ
    }
    fn get_hardware_time(&self) -> Result<u64, Error> {
        Ok(self.clock_start.elapsed().as_nanos() as u64)
    }
    fn rx_activate(&mut self, time_ticks: Option<u64>) -> Result<(), Error> {
        if time_ticks.is_some() {
            return Err(Error::unsupported("timed RX activation"));
        }
        self.active = true;
        Ok(())
    }
    fn stop(&mut self) -> Result<(), Error> {
        self.active = false;
        Ok(())
    }
    fn rx_read(&mut self, out: &mut [Complex32], timeout_us: i64) -> Result<RxReadResult, Error> {
        if !self.active {
            return Err("RX stream is inactive".into());
        }
        if out.is_empty() {
            return Ok(RxReadResult {
                samples_read: 0,
                time_ticks: self.get_hardware_time()?,
                overflow: false,
            });
        }
        let (block, consumed) = match self.pending.take() {
            Some(pending) => pending,
            None => {
                let received = if timeout_us < 0 {
                    self.input.recv().map_err(Error::from)
                } else {
                    self.input
                        .recv_timeout(Duration::from_micros(timeout_us as u64))
                        .map_err(Error::from)
                };
                match received {
                    Ok(block) => (block, 0),
                    Err(error) if error.kind() == crate::error::ErrorKind::Timeout => {
                        return Ok(RxReadResult {
                            samples_read: 0,
                            time_ticks: 0,
                            overflow: false,
                        });
                    }
                    Err(error) => return Err(error),
                }
            }
        };
        let count = out.len().min(block.samples.len() - consumed);
        out[..count].copy_from_slice(&block.samples[consumed..consumed + count]);
        let elapsed_ticks =
            (consumed as f64 * TICK_RATE_HZ as f64 / self.sample_rate_hz).round() as u64;
        let result = RxReadResult {
            samples_read: count,
            time_ticks: block
                .time_ticks
                .unwrap_or(self.get_hardware_time()?)
                .saturating_add(elapsed_ticks),
            overflow: consumed == 0 && block.discontinuity,
        };
        if consumed + count < block.samples.len() {
            self.pending = Some((block, consumed + count));
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn applied_rates_are_available_before_and_after_splitting() {
        use crate::{Radio, RadioTx};
        const TX_RATE_HZ: usize = 2_000_000;
        const RX_RATE_HZ: f64 = 1_999_999.25;
        let (mut radio, _peer) = super::MemoryRadio::new(1);
        assert_eq!(radio.tx_sample_rate_hz(), 0.0);
        assert_eq!(radio.rx_sample_rate_hz(), None);
        radio.set_tx_sample_rate(TX_RATE_HZ as f64).unwrap();
        radio.setup_rx(0, "", RX_RATE_HZ, RX_RATE_HZ, None).unwrap();
        assert_eq!(radio.tx_sample_rate_hz(), TX_RATE_HZ as f64);
        assert_eq!(radio.rx_sample_rate_hz(), Some(RX_RATE_HZ));
        assert!(radio.set_tx_sample_rate(0.0).is_err());
        let (tx, rx) = Box::new(radio).split().unwrap();
        assert_eq!(tx.tx_sample_rate_hz(), TX_RATE_HZ as f64);
        assert_eq!(rx.unwrap().rx_sample_rate_hz(), RX_RATE_HZ);
    }

    use super::*;

    #[test]
    fn arbitrary_rate_iq_roundtrip_preserves_time_and_discontinuity() {
        let (mut radio, handle) = MemoryRadio::new(2);
        let rate_hz = 2_000_000.0;
        assert_eq!(
            radio.setup_rx(0, "", rate_hz, rate_hz, None).unwrap(),
            rate_hz
        );
        let (mut tx, rx) = Box::new(radio).split().unwrap();
        let mut rx = rx.unwrap();
        let samples = vec![Complex32::new(0.5, -0.25); 4];
        tx.transmit_at(&samples, Some(10_000)).unwrap();
        let mut block = handle.output.recv().unwrap();
        assert_eq!(block.samples, samples);
        assert_eq!(block.time_ticks, Some(10_000));
        block.discontinuity = true;
        handle.input.send(block).unwrap();
        rx.rx_activate(None).unwrap();
        let mut out = [Complex32::default(); 2];
        let first = rx.rx_read(&mut out, 0).unwrap();
        assert_eq!(first.samples_read, 2);
        assert_eq!(first.time_ticks, 10_000);
        assert!(first.overflow);
        let second = rx.rx_read(&mut out, 0).unwrap();
        assert_eq!(second.samples_read, 2);
        assert_eq!(second.time_ticks, 11_000);
        assert!(!second.overflow);
        assert_eq!(out, samples[..2]);
    }
}
