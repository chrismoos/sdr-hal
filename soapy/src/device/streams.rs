//! Per-stream state behind a [`Device`](super::Device): the halves of a split session.

use num_complex::Complex32;
use sdr_hal::error::Error;
use sdr_hal::{RadioRx, RadioTx, TxRadioHealth};

use super::ReadOutcome;
use super::time::{samples_to_ticks, ticks_to_ns};

/// Where untimed TX stands against device time, so writes wait instead of overrunning.
#[derive(Debug, Clone, Copy)]
pub(super) struct UntimedPacing {
    pub(super) anchor_ticks: u64,
    pub(super) sent_samples: u64,
}

pub(super) struct TxHalf {
    pub(super) tx: Box<dyn RadioTx>,
    pub(super) rate_hz: f64,
    pub(super) tick_rate: u64,
    pub(super) pacing: Option<UntimedPacing>,
    pub(super) health: TxRadioHealth,
}

impl TxHalf {
    /// Ticks the untimed stream is ahead of device time once `samples` more are sent.
    pub(super) fn untimed_lead_ticks(&mut self, samples: usize) -> Result<u64, Error> {
        let now = self.tx.get_hardware_time()?;
        let pacing = match self.pacing {
            Some(pacing)
                if pacing.anchor_ticks
                    + samples_to_ticks(pacing.sent_samples, self.tick_rate, self.rate_hz)
                    >= now =>
            {
                pacing
            }
            // Behind device time the server restarts the stream from now as well.
            _ => UntimedPacing {
                anchor_ticks: now,
                sent_samples: 0,
            },
        };
        self.pacing = Some(pacing);
        let end = pacing.anchor_ticks
            + samples_to_ticks(
                pacing.sent_samples + samples as u64,
                self.tick_rate,
                self.rate_hz,
            );
        Ok(end.saturating_sub(now))
    }
}

pub(super) struct HeldSamples {
    pub(super) samples: Vec<Complex32>,
    pub(super) time_ticks: u64,
}

pub(super) struct RxHalf {
    pub(super) rx: Box<dyn RadioRx>,
    pub(super) tick_rate: u64,
    pub(super) held: Option<HeldSamples>,
}

impl RxHalf {
    pub(super) fn take_held(&mut self, buf: &mut [Complex32]) -> Option<ReadOutcome> {
        let mut held = self.held.take()?;
        let count = held.samples.len().min(buf.len());
        buf[..count].copy_from_slice(&held.samples[..count]);
        let time_ns = ticks_to_ns(held.time_ticks, self.tick_rate);
        if count < held.samples.len() {
            held.time_ticks +=
                samples_to_ticks(count as u64, self.tick_rate, self.rx.rx_sample_rate_hz());
            held.samples.drain(..count);
            self.held = Some(held);
        }
        Some(ReadOutcome::Samples { count, time_ns })
    }
}
