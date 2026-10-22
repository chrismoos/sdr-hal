//! `extern "C"` functions declared in `soapy/include/sdrhal_soapy.h`.
//!
//! Every function catches panics and records failures for [`sdrhal_last_error`], so no
//! unwind crosses into C++. Status codes equal SoapySDR's own error codes.

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_int, c_long, c_longlong};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Once;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use num_complex::Complex32;
use sdr_hal::error::Error;
use sdr_hal::network::NetworkRadioOptions;
use sdr_hal::network::wire::Transport;

use crate::device::{Device, Direction, ReadOutcome, TxEvent, TxTiming, WriteOutcome};

const SDRHAL_OK: c_int = 0;
/// `SOAPY_SDR_TIMEOUT`.
const SDRHAL_TIMEOUT: c_int = -1;
/// `SOAPY_SDR_STREAM_ERROR`, for any failure with a message in [`sdrhal_last_error`].
const SDRHAL_ERROR: c_int = -2;
/// `SOAPY_SDR_OVERFLOW`.
const SDRHAL_OVERFLOW: c_int = -4;
/// `SOAPY_SDR_TIME_ERROR`.
const SDRHAL_TIME_ERROR: c_int = -6;
/// `SOAPY_SDR_UNDERFLOW`.
const SDRHAL_UNDERFLOW: c_int = -7;

/// `SOAPY_SDR_TX` and `SOAPY_SDR_RX`.
const SOAPY_TX: c_int = 0;
const SOAPY_RX: c_int = 1;

const TRANSPORT_TCP: c_int = 1;

/// `sdrhal_range` in the header. `present` is zero when the server sets no range.
#[repr(C)]
pub struct CRange {
    present: c_int,
    minimum: f64,
    maximum: f64,
}

/// `sdrhal_log_fn` in the header, receiving a level from 1 (error) to 5 (trace) and a NUL-terminated line.
pub type LogHandler = extern "C" fn(level: c_int, message: *const c_char);

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::default());
}

static LOG_HANDLER: AtomicUsize = AtomicUsize::new(0);
static LOGGER_INSTALLED: Once = Once::new();

fn set_last_error(message: &str) {
    let text = CString::new(message.replace('\0', " ")).unwrap_or_default();
    LAST_ERROR.with(|slot| *slot.borrow_mut() = text);
}

/// Runs `body`, recording its error or panic and returning `failed` in its place.
fn guard<T>(failed: T, body: impl FnOnce() -> Result<T, Error>) -> T {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            set_last_error(&error.to_string());
            failed
        }
        Err(panic) => {
            let message = panic
                .downcast_ref::<&str>()
                .map(|text| text.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            set_last_error(&format!("sdrhal: internal error: {message}"));
            failed
        }
    }
}

fn direction(value: c_int) -> Result<Direction, Error> {
    match value {
        SOAPY_TX => Ok(Direction::Tx),
        SOAPY_RX => Ok(Direction::Rx),
        other => Err(format!("unknown stream direction {other}").into()),
    }
}

/// # Safety
/// `device` must be null or a pointer returned by [`sdrhal_open`] and not yet closed.
unsafe fn device<'a>(device: *const Device) -> Result<&'a Device, Error> {
    // SAFETY: the caller passes a live pointer from `sdrhal_open` or null, which is
    // refused here.
    unsafe { device.as_ref() }.ok_or_else(|| Error::from("null sdrhal device"))
}

/// # Safety
/// `text` must be null or a NUL-terminated string valid for the call.
unsafe fn string(text: *const c_char) -> Result<Option<String>, Error> {
    if text.is_null() {
        return Ok(None);
    }
    // SAFETY: non-null and NUL-terminated per the caller's contract.
    let text = unsafe { CStr::from_ptr(text) };
    text.to_str()
        .map(|text| Some(text.to_string()))
        .map_err(|_| Error::from("string argument is not UTF-8"))
}

/// Copies `text` into `buf` with a NUL terminator, truncating to fit, and returns the
/// length the whole string needs without its terminator.
///
/// # Safety
/// `buf` must be null or valid for `len` bytes.
unsafe fn copy_out(text: &str, buf: *mut c_char, len: usize) -> usize {
    if !buf.is_null() && len > 0 {
        let count = text.len().min(len - 1);
        // SAFETY: `buf` holds `len` bytes and `count + 1 <= len`.
        unsafe {
            std::ptr::copy_nonoverlapping(text.as_ptr().cast::<c_char>(), buf, count);
            *buf.add(count) = 0;
        }
    }
    text.len()
}

fn timeout(timeout_us: c_long) -> Duration {
    Duration::from_micros(u64::try_from(timeout_us).unwrap_or(0))
}

fn status(body: impl FnOnce() -> Result<(), Error>) -> c_int {
    guard(SDRHAL_ERROR, || body().map(|()| SDRHAL_OK))
}

struct HandlerLogger;

impl log::Log for HandlerLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record) {
        let handler = LOG_HANDLER.load(Ordering::Acquire);
        if handler == 0 || !self.enabled(record.metadata()) {
            return;
        }
        // SAFETY: only `sdrhal_set_log_handler` stores a nonzero value, and it stores a
        // valid `LogHandler`.
        let handler: LogHandler = unsafe { std::mem::transmute(handler) };
        let line = CString::new(record.args().to_string().replace('\0', " ")).unwrap_or_default();
        handler(record.level() as c_int, line.as_ptr());
    }

    fn flush(&self) {}
}

/// Message for the last failure on this thread. Valid until this thread's next call.
#[unsafe(no_mangle)]
pub extern "C" fn sdrhal_last_error() -> *const c_char {
    LAST_ERROR.with(|slot| slot.borrow().as_ptr())
}

/// Sends this library's log lines to `handler`, or stops sending them when it is null.
/// `SDRHAL_LOG` selects the level by name and defaults to `info`.
#[unsafe(no_mangle)]
pub extern "C" fn sdrhal_set_log_handler(handler: Option<LogHandler>) {
    LOG_HANDLER.store(
        handler.map_or(0, |handler| handler as usize),
        Ordering::Release,
    );
    LOGGER_INSTALLED.call_once(|| {
        if log::set_logger(&HandlerLogger).is_ok() {
            let level = std::env::var("SDRHAL_LOG")
                .ok()
                .and_then(|level| level.parse().ok())
                .unwrap_or(log::LevelFilter::Info);
            log::set_max_level(level);
        }
    });
}

/// Connects to `addr` (`host:port`). `transport` is 0 for UDP and 1 for TCP. Zero
/// `samples_per_packet` and a null `data_host` take the defaults. Returns null on failure.
///
/// # Safety
/// `addr` must be a NUL-terminated string and `data_host` null or one.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_open(
    addr: *const c_char,
    transport: c_int,
    samples_per_packet: usize,
    data_host: *const c_char,
) -> *mut Device {
    guard(std::ptr::null_mut(), || {
        // SAFETY: forwarded from the caller's contract.
        let addr = unsafe { string(addr) }?.ok_or_else(|| Error::from("null address"))?;
        let defaults = NetworkRadioOptions::default();
        let options = NetworkRadioOptions {
            transport: if transport == TRANSPORT_TCP {
                Transport::Tcp
            } else {
                Transport::Udp
            },
            samples_per_packet: if samples_per_packet == 0 {
                defaults.samples_per_packet
            } else {
                samples_per_packet
            },
            // SAFETY: forwarded from the caller's contract.
            data_host: unsafe { string(data_host) }?,
        };
        Ok(Box::into_raw(Box::new(Device::open(&addr, options)?)))
    })
}

/// Ends the session and frees the device.
///
/// # Safety
/// `device` must be null or from [`sdrhal_open`], and no other call may use it after.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_close(device: *mut Device) {
    if device.is_null() {
        return;
    }
    guard((), || {
        // SAFETY: the caller hands over the pointer `sdrhal_open` returned.
        drop(unsafe { Box::from_raw(device) });
        Ok(())
    });
}

/// Copies the server backend name into `buf` and returns its full length.
///
/// # Safety
/// `device` as for [`sdrhal_close`], and `buf` null or valid for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_backend(
    device: *const Device,
    buf: *mut c_char,
    len: usize,
) -> usize {
    guard(0, || {
        // SAFETY: forwarded from the caller's contract.
        let device = unsafe { self::device(device) }?;
        // SAFETY: forwarded from the caller's contract.
        Ok(unsafe { copy_out(device.backend(), buf, len) })
    })
}

/// Copies the control address into `buf` and returns its full length.
///
/// # Safety
/// As for [`sdrhal_backend`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_addr(device: *const Device, buf: *mut c_char, len: usize) -> usize {
    guard(0, || {
        // SAFETY: forwarded from the caller's contract.
        let device = unsafe { self::device(device) }?;
        // SAFETY: forwarded from the caller's contract.
        Ok(unsafe { copy_out(device.addr(), buf, len) })
    })
}

/// One when the server can receive.
///
/// # Safety
/// `device` as for [`sdrhal_close`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_supports_rx(device: *const Device) -> c_int {
    // SAFETY: forwarded from the caller's contract.
    guard(0, || {
        Ok(c_int::from(unsafe { self::device(device) }?.supports_rx()))
    })
}

/// Device clock ticks per second.
///
/// # Safety
/// `device` as for [`sdrhal_close`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_tick_rate(device: *const Device) -> u64 {
    // SAFETY: forwarded from the caller's contract.
    guard(0, || Ok(unsafe { self::device(device) }?.tick_rate()))
}

/// Complex samples per IQ packet.
///
/// # Safety
/// `device` as for [`sdrhal_close`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_samples_per_packet(device: *const Device) -> usize {
    // SAFETY: forwarded from the caller's contract.
    guard(0, || {
        Ok(unsafe { self::device(device) }?.samples_per_packet())
    })
}

/// Fills the sample-rate and frequency ranges the server accepts for `direction`.
///
/// # Safety
/// `device` as for [`sdrhal_close`], and both outputs valid for one write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_ranges(
    device: *const Device,
    direction: c_int,
    rate: *mut CRange,
    frequency: *mut CRange,
) -> c_int {
    guard(SDRHAL_ERROR, || {
        // SAFETY: forwarded from the caller's contract.
        let device = unsafe { self::device(device) }?;
        let ranges = device.ranges();
        let (rate_range, frequency_range) = match self::direction(direction)? {
            Direction::Tx => (&ranges.tx_sample_rate_hz, &ranges.tx_frequency_hz),
            Direction::Rx => (&ranges.rx_sample_rate_hz, &ranges.rx_frequency_hz),
        };
        let to_c = |range: &Option<std::ops::RangeInclusive<f64>>| match range {
            Some(range) => CRange {
                present: 1,
                minimum: *range.start(),
                maximum: *range.end(),
            },
            None => CRange {
                present: 0,
                minimum: 0.0,
                maximum: 0.0,
            },
        };
        // SAFETY: both pointers are valid for one write per the caller's contract.
        unsafe {
            rate.write(to_c(rate_range));
            frequency.write(to_c(frequency_range));
        }
        Ok(SDRHAL_OK)
    })
}

/// Sets the sample rate and writes the applied rate to `applied_hz` when non-null.
///
/// # Safety
/// `device` as for [`sdrhal_close`], and `applied_hz` null or valid for one write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_set_sample_rate(
    device: *const Device,
    direction: c_int,
    rate_hz: f64,
    applied_hz: *mut f64,
) -> c_int {
    guard(SDRHAL_ERROR, || {
        // SAFETY: forwarded from the caller's contract.
        let device = unsafe { self::device(device) }?;
        let applied = device.set_sample_rate(self::direction(direction)?, rate_hz)?;
        if !applied_hz.is_null() {
            // SAFETY: non-null and valid for one write per the caller's contract.
            unsafe { applied_hz.write(applied) };
        }
        Ok(SDRHAL_OK)
    })
}

/// Applied sample rate in Hz, zero before one is set.
///
/// # Safety
/// `device` as for [`sdrhal_close`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_sample_rate(device: *const Device, direction: c_int) -> f64 {
    guard(0.0, || {
        // SAFETY: forwarded from the caller's contract.
        Ok(unsafe { self::device(device) }?.sample_rate(self::direction(direction)?))
    })
}

/// Tunes the center frequency in Hz.
///
/// # Safety
/// `device` as for [`sdrhal_close`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_set_frequency(
    device: *const Device,
    direction: c_int,
    frequency_hz: f64,
) -> c_int {
    status(|| {
        // SAFETY: forwarded from the caller's contract.
        unsafe { self::device(device) }?.set_frequency(self::direction(direction)?, frequency_hz)
    })
}

/// Requested center frequency in Hz, zero before one is set.
///
/// # Safety
/// `device` as for [`sdrhal_close`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_frequency(device: *const Device, direction: c_int) -> f64 {
    guard(0.0, || {
        // SAFETY: forwarded from the caller's contract.
        Ok(unsafe { self::device(device) }?.frequency(self::direction(direction)?))
    })
}

/// Sets the filter bandwidth in Hz.
///
/// # Safety
/// `device` as for [`sdrhal_close`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_set_bandwidth(
    device: *const Device,
    direction: c_int,
    bandwidth_hz: f64,
) -> c_int {
    status(|| {
        // SAFETY: forwarded from the caller's contract.
        unsafe { self::device(device) }?.set_bandwidth(self::direction(direction)?, bandwidth_hz)
    })
}

/// Requested filter bandwidth in Hz, zero before one is set.
///
/// # Safety
/// `device` as for [`sdrhal_close`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_bandwidth(device: *const Device, direction: c_int) -> f64 {
    guard(0.0, || {
        // SAFETY: forwarded from the caller's contract.
        Ok(unsafe { self::device(device) }?.bandwidth(self::direction(direction)?))
    })
}

/// Sets the RX gain in dB.
///
/// # Safety
/// `device` as for [`sdrhal_close`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_set_rx_gain(device: *const Device, gain_db: f64) -> c_int {
    // SAFETY: forwarded from the caller's contract.
    status(|| unsafe { self::device(device) }?.set_rx_gain(gain_db))
}

/// TX gain range in dB. `present` is zero when the server maintains its configured TX gain.
///
/// # Safety
/// `device` as for [`sdrhal_close`], and `range` valid for one write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_tx_gain_range(device: *const Device, range: *mut CRange) -> c_int {
    status(|| {
        // SAFETY: forwarded from the caller's contract.
        let found = unsafe { self::device(device) }?.tx_gain_range();
        let value = match found {
            Some((minimum, maximum)) => CRange {
                present: 1,
                minimum,
                maximum,
            },
            None => CRange {
                present: 0,
                minimum: 0.0,
                maximum: 0.0,
            },
        };
        // SAFETY: valid for one write per the caller's contract.
        unsafe { range.write(value) };
        Ok(())
    })
}

/// Sets the TX gain in dB and writes the applied gain to `applied_db`.
///
/// # Safety
/// `device` as for [`sdrhal_close`], and `applied_db` valid for one write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_set_tx_gain(
    device: *const Device,
    gain_db: f64,
    applied_db: *mut f64,
) -> c_int {
    status(|| {
        // SAFETY: forwarded from the caller's contract.
        let applied = unsafe { self::device(device) }?.set_tx_gain(gain_db)?;
        // SAFETY: valid for one write per the caller's contract.
        unsafe { applied_db.write(applied) };
        Ok(())
    })
}

/// Applied TX gain in dB, or zero when the server maintains its configured TX gain.
///
/// # Safety
/// `device` as for [`sdrhal_close`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_tx_gain(device: *const Device) -> f64 {
    // SAFETY: forwarded from the caller's contract.
    guard(0.0, || {
        Ok(unsafe { self::device(device) }?.tx_gain().unwrap_or(0.0))
    })
}

/// Requested RX gain in dB.
///
/// # Safety
/// `device` as for [`sdrhal_close`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_rx_gain(device: *const Device) -> f64 {
    // SAFETY: forwarded from the caller's contract.
    guard(0.0, || Ok(unsafe { self::device(device) }?.rx_gain()))
}

/// Selects the RX port by name. An empty name selects the server default.
///
/// # Safety
/// `device` as for [`sdrhal_close`], and `name` a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_set_rx_antenna(
    device: *const Device,
    name: *const c_char,
) -> c_int {
    status(|| {
        // SAFETY: forwarded from the caller's contract.
        let name = unsafe { string(name) }?.unwrap_or_default();
        // SAFETY: forwarded from the caller's contract.
        unsafe { self::device(device) }?.set_rx_antenna(&name)
    })
}

/// Copies the requested RX port into `buf` and returns its full length.
///
/// # Safety
/// As for [`sdrhal_backend`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_rx_antenna(
    device: *const Device,
    buf: *mut c_char,
    len: usize,
) -> usize {
    guard(0, || {
        // SAFETY: forwarded from the caller's contract.
        let antenna = unsafe { self::device(device) }?.rx_antenna();
        // SAFETY: forwarded from the caller's contract.
        Ok(unsafe { copy_out(&antenna, buf, len) })
    })
}

/// Requests the device clock rate and writes the applied rate when `applied` is non-null.
///
/// # Safety
/// `device` as for [`sdrhal_close`], and `applied` null or valid for one write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_set_tick_rate(
    device: *const Device,
    tick_rate: u64,
    applied: *mut u64,
) -> c_int {
    guard(SDRHAL_ERROR, || {
        // SAFETY: forwarded from the caller's contract.
        let rate = unsafe { self::device(device) }?.set_tick_rate(tick_rate)?;
        if !applied.is_null() {
            // SAFETY: non-null and valid for one write per the caller's contract.
            unsafe { applied.write(rate) };
        }
        Ok(SDRHAL_OK)
    })
}

/// Declares a stream. Every stream must be set up before the first is activated.
///
/// # Safety
/// `device` as for [`sdrhal_close`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_setup_stream(device: *const Device, direction: c_int) -> c_int {
    // SAFETY: forwarded from the caller's contract.
    status(|| unsafe { self::device(device) }?.setup_stream(self::direction(direction)?))
}

/// Releases a stream, ending the session with the last one.
///
/// # Safety
/// `device` as for [`sdrhal_close`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_close_stream(device: *const Device, direction: c_int) -> c_int {
    // SAFETY: forwarded from the caller's contract.
    status(|| unsafe { self::device(device) }?.close_stream(self::direction(direction)?))
}

/// Starts a stream, at `time_ns` device time when `has_time` is nonzero.
///
/// # Safety
/// `device` as for [`sdrhal_close`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_activate(
    device: *const Device,
    direction: c_int,
    has_time: c_int,
    time_ns: c_longlong,
) -> c_int {
    status(|| {
        let time = (has_time != 0).then_some(time_ns);
        // SAFETY: forwarded from the caller's contract.
        unsafe { self::device(device) }?.activate(self::direction(direction)?, time)
    })
}

/// Stops a stream.
///
/// # Safety
/// `device` as for [`sdrhal_close`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_deactivate(device: *const Device, direction: c_int) -> c_int {
    // SAFETY: forwarded from the caller's contract.
    status(|| unsafe { self::device(device) }?.deactivate(self::direction(direction)?))
}

/// Reads up to `samples` interleaved complex floats into `iq`. Returns the count read
/// and writes the first sample's device time to `time_ns`, or returns a negative status.
///
/// # Safety
/// `device` as for [`sdrhal_close`], `iq` valid for `2 * samples` floats, and `time_ns`
/// valid for one write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_read(
    device: *const Device,
    iq: *mut f32,
    samples: usize,
    timeout_us: c_long,
    time_ns: *mut c_longlong,
) -> c_long {
    guard(SDRHAL_ERROR as c_long, || {
        // SAFETY: forwarded from the caller's contract.
        let device = unsafe { self::device(device) }?;
        if samples == 0 {
            return Ok(0);
        }
        // SAFETY: `Complex32` is `repr(C)` with two `f32` fields, so `samples` of them
        // occupy the `2 * samples` floats the caller provides, at float alignment.
        let buf = unsafe { std::slice::from_raw_parts_mut(iq.cast::<Complex32>(), samples) };
        Ok(match device.read(buf, timeout(timeout_us))? {
            ReadOutcome::Samples {
                count,
                time_ns: first_ns,
            } => {
                // SAFETY: valid for one write per the caller's contract.
                unsafe { time_ns.write(first_ns) };
                count as c_long
            }
            ReadOutcome::Overflow => SDRHAL_OVERFLOW as c_long,
            ReadOutcome::Timeout => SDRHAL_TIMEOUT as c_long,
        })
    })
}

/// Sends `samples` interleaved complex floats from `iq`, at `time_ns` when `has_time` is
/// nonzero, ending the burst when `end_of_burst` is nonzero. Returns the count sent or a
/// negative status.
///
/// # Safety
/// `device` as for [`sdrhal_close`], and `iq` valid for `2 * samples` floats.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_write(
    device: *const Device,
    iq: *const f32,
    samples: usize,
    has_time: c_int,
    time_ns: c_longlong,
    end_of_burst: c_int,
    timeout_us: c_long,
) -> c_long {
    guard(SDRHAL_ERROR as c_long, || {
        // SAFETY: forwarded from the caller's contract.
        let device = unsafe { self::device(device) }?;
        if samples == 0 {
            return Ok(0);
        }
        // SAFETY: as in `sdrhal_read`, for a buffer that is only read.
        let buf = unsafe { std::slice::from_raw_parts(iq.cast::<Complex32>(), samples) };
        let timing = if has_time != 0 {
            TxTiming::At(time_ns)
        } else {
            TxTiming::Untimed
        };
        Ok(
            match device.write(buf, timing, end_of_burst != 0, timeout(timeout_us))? {
                WriteOutcome::Written(count) => count as c_long,
                WriteOutcome::Timeout => SDRHAL_TIMEOUT as c_long,
            },
        )
    })
}

/// Waits for a TX event and returns its status, or the timeout status when none came.
///
/// # Safety
/// `device` as for [`sdrhal_close`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_tx_event(device: *const Device, timeout_us: c_long) -> c_int {
    guard(SDRHAL_ERROR, || {
        // SAFETY: forwarded from the caller's contract.
        let device = unsafe { self::device(device) }?;
        Ok(match device.tx_event(timeout(timeout_us))? {
            TxEvent::Underflow => SDRHAL_UNDERFLOW,
            TxEvent::Late => SDRHAL_TIME_ERROR,
            TxEvent::Timeout => SDRHAL_TIMEOUT,
        })
    })
}

/// Writes device time in nanoseconds to `time_ns`.
///
/// # Safety
/// `device` as for [`sdrhal_close`], and `time_ns` valid for one write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_hardware_time(
    device: *const Device,
    time_ns: *mut c_longlong,
) -> c_int {
    guard(SDRHAL_ERROR, || {
        // SAFETY: forwarded from the caller's contract.
        let now = unsafe { self::device(device) }?.hardware_time_ns()?;
        // SAFETY: valid for one write per the caller's contract.
        unsafe { time_ns.write(now) };
        Ok(SDRHAL_OK)
    })
}

/// Resets device time to `time_ns`.
///
/// # Safety
/// `device` as for [`sdrhal_close`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sdrhal_set_hardware_time(
    device: *const Device,
    time_ns: c_longlong,
) -> c_int {
    // SAFETY: forwarded from the caller's contract.
    status(|| unsafe { self::device(device) }?.set_hardware_time_ns(time_ns))
}
