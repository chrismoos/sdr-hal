//! A receive stream whose samples and timestamps follow disciplined ticks.

use std::sync::Arc;

use num_complex::Complex32;

use super::interp::{Kernel, TAPS, Window};
use super::map::{MapView, Position};
use super::{Shared, rotate};
use crate::error::Error;
use crate::{RadioRx, RxReadResult};

/// The shortest read buffer the stream can fill.
const MIN_READ: usize = 2 * TAPS;
/// A read leaves this much of the buffer free, because a slow device clock yields more
/// output samples than the device samples read.
const READ_MARGIN: usize = 2;

struct Stream {
    /// Device time of device sample zero.
    device_origin: Position,
    /// Device samples received since `device_origin`.
    received: i64,
    /// Disciplined time of the next output sample.
    output_origin: Position,
}

pub(crate) struct DisciplinedRx {
    inner: Box<dyn RadioRx>,
    shared: Arc<Shared>,
    view: MapView,
    kernel: Arc<Kernel>,
    window: Window,
    carrier_hz: Option<f64>,
    stream: Option<Stream>,
    raw: Vec<Complex32>,
}

impl DisciplinedRx {
    pub(crate) fn new(
        inner: Box<dyn RadioRx>,
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
            raw: Vec::new(),
        }
    }
}

impl RadioRx for DisciplinedRx {
    fn rx_sample_rate_hz(&self) -> f64 {
        super::client_rate_hz(self.inner.rx_sample_rate_hz())
    }

    fn rx_reference_dbm(&self) -> Option<f32> {
        self.inner.rx_reference_dbm()
    }

    fn tick_rate(&self) -> u64 {
        self.inner.tick_rate()
    }

    fn get_hardware_time(&self) -> Result<u64, Error> {
        self.shared
            .disciplined_ticks(self.inner.get_hardware_time()?)
    }

    fn rx_read(&mut self, buf: &mut [Complex32], timeout_us: i64) -> Result<RxReadResult, Error> {
        if buf.len() < MIN_READ {
            return Err(Error::invalid_setting(format!(
                "a disciplined RX read needs room for {MIN_READ} samples"
            )));
        }
        let want = buf.len() - READ_MARGIN;
        self.raw.resize(want, Complex32::new(0.0, 0.0));
        let read = self.inner.rx_read(&mut self.raw[..want], timeout_us)?;
        let count = read.samples_read.min(want);
        if count == 0 {
            return Ok(read);
        }
        self.view.refresh();
        let tick_rate = self.inner.tick_rate() as f64;
        let ticks_per_sample = tick_rate / self.inner.rx_sample_rate_hz();
        let client_ticks = tick_rate / super::client_rate_hz(self.inner.rx_sample_rate_hz());
        let arrival = Position::from_ticks(read.time_ticks);
        let continues = !read.overflow
            && self.stream.as_ref().is_some_and(|stream| {
                let expected = stream
                    .device_origin
                    .offset(stream.received as f64 * ticks_per_sample);
                arrival.minus(expected).abs() <= ticks_per_sample / 2.0
            });
        let resumed = !continues && self.stream.is_some();
        if resumed {
            log::warn!(
                "clock: RX stream restarted at device tick {} overflow={}",
                read.time_ticks,
                read.overflow
            );
        }
        if !continues {
            self.window.restart();
            self.stream = Some(Stream {
                device_origin: arrival,
                received: 0,
                output_origin: Position::from_ticks(self.view.disciplined_at(arrival).ceil()),
            });
        }
        let stream = self.stream.as_mut().expect("stream was just started");

        if let Some(carrier_hz) = self.carrier_hz {
            let mut done = 0;
            while done < count {
                let device = stream
                    .device_origin
                    .offset((stream.received + done as i64) as f64 * ticks_per_sample);
                let (rate, boundary) = self.view.rate_at_device(device.whole);
                let piece = boundary.map_or(count - done, |boundary| {
                    ((Position::from_ticks(boundary).minus(device) / ticks_per_sample)
                        .ceil()
                        .max(1.0) as usize)
                        .min(count - done)
                });
                // The receive oscillator runs fast by the same ratio as the clock, which
                // leaves the signal low in the baseband.
                rotate(
                    &mut self.raw[done..done + piece],
                    carrier_hz,
                    device.minus(self.view.disciplined_at(device)),
                    rate,
                    ticks_per_sample,
                    tick_rate,
                );
                done += piece;
            }
        }
        self.window.push(&self.raw[..count]);
        stream.received += count as i64;

        let first_output = stream.output_origin;
        let mut written = 0;
        while written < buf.len() {
            let disciplined = stream.output_origin.offset(written as f64 * client_ticks);
            let device = self.view.device_at(disciplined);
            let position = device.minus(stream.device_origin) / ticks_per_sample;
            let last = self.window.last_readable();
            if position > last {
                break;
            }
            let (rate, boundary) = self.view.rate_at_device(device.whole);
            let step = client_ticks / ticks_per_sample * (1.0 + rate);
            let by_input = ((last - position) / step).floor() as usize + 1;
            let by_boundary = boundary.map_or(usize::MAX, |boundary| {
                (Position::from_ticks(boundary).minus(device) / (ticks_per_sample * step))
                    .ceil()
                    .max(1.0) as usize
            });
            let piece = by_input.min(by_boundary).min(buf.len() - written);
            self.window.read_run(
                &self.kernel,
                position,
                step,
                &mut buf[written..written + piece],
            );
            written += piece;
        }
        stream.output_origin = stream.output_origin.offset(written as f64 * client_ticks);
        let next = self
            .view
            .device_at(stream.output_origin)
            .minus(stream.device_origin)
            / ticks_per_sample;
        self.window.discard_before(next);
        let shift = next.floor() as i64 - 1;
        if shift > 0 {
            stream.device_origin = stream.device_origin.offset(shift as f64 * ticks_per_sample);
            stream.received -= shift;
            self.window.renumber(shift);
        }
        Ok(RxReadResult {
            samples_read: written,
            time_ticks: first_output.round(),
            overflow: read.overflow || resumed,
        })
    }

    fn rx_activate(&mut self, time_ticks: Option<u64>) -> Result<(), Error> {
        self.view.refresh();
        self.stream = None;
        self.inner.rx_activate(
            time_ticks.map(|ticks| self.view.device_at(Position::from_ticks(ticks)).round()),
        )
    }

    fn stop(&mut self) -> Result<(), Error> {
        self.stream = None;
        self.inner.stop()
    }

    fn set_rx_frequency(&mut self, frequency_hz: f64) -> Result<(), Error> {
        self.inner.set_rx_frequency(frequency_hz)?;
        self.carrier_hz = Some(frequency_hz);
        Ok(())
    }

    fn set_rx_gain(&mut self, gain_db: f64) -> Result<(), Error> {
        self.inner.set_rx_gain(gain_db)
    }
}
