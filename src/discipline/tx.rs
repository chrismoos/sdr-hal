//! A transmit stream whose timestamps count disciplined ticks.

use std::sync::Arc;

use num_complex::Complex32;

use super::interp::{AFTER, Kernel, Window};
use super::map::{MapView, Position};
use super::{Shared, rotate};
use crate::error::Error;
use crate::{RadioClock, RadioTx, Schedule, TxCapabilities, TxCapability, TxMetadata};

/// The samples of a timed stream in progress. Both origins move forward as samples are
/// sent, so sample indexes stay small however long the stream runs.
struct Stream {
    /// Disciplined time of input sample zero.
    input_origin: Position,
    /// Input samples received since `input_origin`.
    inputs: i64,
    /// Device time of the next output sample.
    device_origin: Position,
}

pub(crate) struct DisciplinedTx {
    inner: Box<dyn RadioTx>,
    shared: Arc<Shared>,
    view: MapView,
    kernel: Arc<Kernel>,
    window: Window,
    carrier_hz: Option<f64>,
    stream: Option<Stream>,
    out: Vec<Complex32>,
}

impl DisciplinedTx {
    pub(crate) fn new(
        inner: Box<dyn RadioTx>,
        shared: Arc<Shared>,
        kernel: Arc<Kernel>,
        carrier_hz: Option<f64>,
    ) -> Self {
        let view = MapView::new(Arc::clone(&shared.map));
        Self {
            inner,
            shared,
            view,
            kernel,
            window: Window::new(),
            carrier_hz,
            stream: None,
            out: Vec::new(),
        }
    }

    /// Ticks between two samples the client writes.
    fn ticks_per_sample(&self) -> f64 {
        self.inner.tick_rate() as f64 / super::client_rate_hz(self.inner.tx_sample_rate_hz())
    }

    fn start(&mut self, tick: u64) {
        let input_origin = Position::from_ticks(tick);
        let device_origin = Position::from_ticks(self.view.device_at(input_origin).round());
        self.window.restart();
        self.stream = Some(Stream {
            input_origin,
            inputs: 0,
            device_origin,
        });
    }

    /// Sends every output sample the received input covers. `finish` sends the samples
    /// that were waiting for later input and ends the burst.
    fn send_ready(&mut self, finish: bool) -> Result<(), Error> {
        let Some(stream) = self.stream.as_mut() else {
            return Ok(());
        };
        if finish {
            self.window.push(&[Complex32::new(0.0, 0.0); AFTER + 1]);
        }
        let tick_rate = self.inner.tick_rate() as f64;
        let device_ticks = tick_rate / self.inner.tx_sample_rate_hz();
        let client_ticks = tick_rate / super::client_rate_hz(self.inner.tx_sample_rate_hz());
        let first_device = stream.device_origin;
        self.out.clear();
        loop {
            let device = stream
                .device_origin
                .offset(self.out.len() as f64 * device_ticks);
            let disciplined = self.view.disciplined_at(device);
            let position = disciplined.minus(stream.input_origin) / client_ticks;
            let last = self.window.last_readable();
            if position > last {
                break;
            }
            let (rate, boundary) = self.view.rate_at_device(device.whole);
            let step = device_ticks / client_ticks / (1.0 + rate);
            let by_input = ((last - position) / step).floor() as usize + 1;
            let by_boundary = boundary.map_or(usize::MAX, |boundary| {
                (Position::from_ticks(boundary).minus(device) / device_ticks)
                    .ceil()
                    .max(1.0) as usize
            });
            let count = by_input.min(by_boundary);
            let start = self.out.len();
            self.out.resize(start + count, Complex32::new(0.0, 0.0));
            self.window
                .read_run(&self.kernel, position, step, &mut self.out[start..]);
            if let Some(carrier_hz) = self.carrier_hz {
                // The transmit oscillator runs fast by the same ratio as the clock, so
                // the baseband moves down to keep the carrier where it was requested.
                rotate(
                    &mut self.out[start..],
                    -carrier_hz,
                    device.minus(disciplined),
                    rate,
                    device_ticks,
                    tick_rate,
                );
            }
        }
        let sent = self.out.len();
        if sent > 0 {
            self.inner.write_all(
                &self.out,
                TxMetadata {
                    schedule: Schedule::At(crate::DeviceTime::from_ticks(first_device.round())),
                    end_of_burst: finish,
                },
            )?;
        } else if finish {
            self.inner.end_burst()?;
        }
        if finish {
            self.stream = None;
            return Ok(());
        }
        stream.device_origin = stream.device_origin.offset(sent as f64 * device_ticks);
        let next = self
            .view
            .disciplined_at(stream.device_origin)
            .minus(stream.input_origin)
            / client_ticks;
        self.window.discard_before(next);
        let shift = next.floor() as i64 - 1;
        if shift > 0 {
            stream.input_origin = stream.input_origin.offset(shift as f64 * client_ticks);
            stream.inputs -= shift;
            self.window.renumber(shift);
        }
        Ok(())
    }
}

pub(crate) struct DisciplinedClock {
    pub(crate) inner: Box<dyn RadioClock>,
    pub(crate) shared: Arc<Shared>,
}

impl RadioClock for DisciplinedClock {
    fn get_hardware_time(&self) -> Result<u64, Error> {
        self.shared
            .disciplined_ticks(self.inner.get_hardware_time()?)
    }

    fn time_reference(&self) -> Result<Option<crate::TimeReference>, Error> {
        Ok(Some(self.shared.status.time_reference()))
    }

    fn clock_status(&self) -> Option<crate::ClockStatus> {
        Some(self.shared.status.status())
    }
}

impl RadioTx for DisciplinedTx {
    fn clock_reader(&self) -> Option<Box<dyn RadioClock>> {
        let inner = self.inner.clock_reader()?;
        Some(Box::new(DisciplinedClock {
            inner,
            shared: Arc::clone(&self.shared),
        }))
    }

    fn time_reference(&self) -> Result<Option<crate::TimeReference>, Error> {
        Ok(Some(self.shared.status.time_reference()))
    }

    fn clock_status(&self) -> Option<crate::ClockStatus> {
        Some(self.shared.status.status())
    }

    fn capabilities(&self) -> TxCapabilities {
        // Resetting the device clock would move every pulse the map is anchored to.
        let inner = self.inner.capabilities();
        [TxCapability::TimedSamples, TxCapability::BurstEnd]
            .into_iter()
            .filter(|capability| inner.supports(*capability))
            .fold(TxCapabilities::default(), TxCapabilities::with)
    }

    fn write(&mut self, samples: &[Complex32], metadata: TxMetadata) -> Result<usize, Error> {
        match metadata.schedule {
            Schedule::At(time) => {
                self.view.refresh();
                let ticks_per_sample = self.ticks_per_sample();
                let continues = self.stream.as_ref().is_some_and(|stream| {
                    let expected = stream
                        .input_origin
                        .offset(stream.inputs as f64 * ticks_per_sample);
                    Position::from_ticks(time.ticks()).minus(expected).abs()
                        <= ticks_per_sample / 2.0
                });
                if !continues {
                    if let Some(stream) = &self.stream {
                        let expected = stream
                            .input_origin
                            .offset(stream.inputs as f64 * ticks_per_sample);
                        log::warn!(
                            "clock: TX stream restarted, a write came {:.1} ticks from where the last one ended",
                            Position::from_ticks(time.ticks()).minus(expected)
                        );
                    }
                    self.start(time.ticks());
                }
            }
            Schedule::Immediate if self.stream.is_none() => {
                return self.inner.write(samples, metadata);
            }
            Schedule::Immediate => self.view.refresh(),
        }
        self.window.push(samples);
        if let Some(stream) = self.stream.as_mut() {
            stream.inputs += samples.len() as i64;
        }
        self.send_ready(metadata.end_of_burst)?;
        Ok(samples.len())
    }

    fn end_burst(&mut self) -> Result<(), Error> {
        if self.stream.is_some() {
            self.view.refresh();
            self.send_ready(true)
        } else {
            self.inner.end_burst()
        }
    }

    fn tx_sample_rate_hz(&self) -> f64 {
        super::client_rate_hz(self.inner.tx_sample_rate_hz())
    }

    fn rx_to_tx_delay_ns(&self) -> Option<i64> {
        self.inner.rx_to_tx_delay_ns()
    }

    fn tx_full_scale_power_estimate_dbm(&self) -> Option<f32> {
        self.inner.tx_full_scale_power_estimate_dbm()
    }

    fn supports_timed_tx(&self) -> bool {
        self.inner.supports_timed_tx()
    }

    fn tick_rate(&self) -> u64 {
        self.inner.tick_rate()
    }

    fn get_hardware_time(&self) -> Result<u64, Error> {
        self.shared
            .disciplined_ticks(self.inner.get_hardware_time()?)
    }

    fn set_hardware_time(&self, _ticks: u64) -> Result<(), Error> {
        Err(Error::unsupported(
            "setting the time of a disciplined clock",
        ))
    }

    fn transmit(&mut self, samples: &[Complex32]) -> Result<(), Error> {
        self.transmit_at(samples, None)
    }

    fn prepare_transmit(&mut self, max_samples: usize) -> Result<(), Error> {
        let capacity = max_samples + 2 * AFTER;
        self.out.reserve(capacity.saturating_sub(self.out.len()));
        self.inner.prepare_transmit(capacity)
    }

    fn transmit_at(&mut self, samples: &[Complex32], tick: Option<u64>) -> Result<(), Error> {
        let schedule = tick.map_or(Schedule::Immediate, |ticks| {
            Schedule::At(crate::DeviceTime::from_ticks(ticks))
        });
        self.write(
            samples,
            TxMetadata {
                schedule,
                end_of_burst: false,
            },
        )
        .map(|_| ())
    }

    fn enable_transmit(&mut self, enable: bool) -> Result<(), Error> {
        if !enable {
            self.stream = None;
        }
        self.inner.enable_transmit(enable)
    }

    fn enable_transmit_at(&mut self, enable: bool, tick: Option<u64>) -> Result<(), Error> {
        if tick.is_some() {
            return Err(Error::unsupported("timed TX activation"));
        }
        self.enable_transmit(enable)
    }

    fn tx_health(&mut self) -> Result<crate::TxRadioHealth, Error> {
        self.inner.tx_health()
    }

    fn set_tx_frequency_hz(&mut self, frequency_hz: f64) -> Result<(), Error> {
        self.inner.set_tx_frequency_hz(frequency_hz)?;
        self.carrier_hz = Some(frequency_hz);
        Ok(())
    }

    fn set_tx_gain(&mut self, gain_db: f64) -> Result<f64, Error> {
        self.inner.set_tx_gain(gain_db)
    }
}
