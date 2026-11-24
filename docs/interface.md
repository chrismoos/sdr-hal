# Interface

## Radios and halves

A `Radio` is the device before streaming starts. Rates, channels, antennas and
bandwidth are set on it. `split()` consumes it and returns a `RadioTx` and an
optional `RadioRx`. The two halves are independent and can run on separate
threads.

Frequency is set after the split, with `RadioTx::set_tx_frequency_hz` and
`RadioRx::set_rx_frequency`. Opening a radio does not set a frequency, and
neither does preparing a stream. After `setup_rx`, the RX stream stays inactive
until it is activated.

The waveform belongs to the application. Samples are read and written as
`Complex32` at the final rate and are never filtered, resampled or scaled. Full
scale is ±1.0 on each component.

Error handling should branch on `Error::kind()`, not on message text.

## Sample rates

A driver applies the requested rate when the hardware supports it. Some round
it to the nearest supported rate, so the application must use the applied rate
that each setter returns.

- `set_tx_sample_rate` takes Hz as `f64` and returns the applied rate. bladeRF
  rounds a fractional rate to whole Hz. File output rejects one.
- `setup_rx` returns the rate the hardware applied.
- `tick_rate()` returns the clock that timestamps count in, which can differ
  from the sample rate.

Frequencies, rates and bandwidths must be set by the application. The exception
is the USRP master clock. When `master_clock_rate` is omitted, the largest
supported clock that every configured rate divides evenly is selected, so each
sample spans a whole number of ticks. When it is set, it is used as given, and a
rate that does not divide it evenly is an error at open time.

UHD applies TX frequency correction in software by default, because UHD's own
correction causes phase jumps when late samples interrupt TX playback.
`software_tx_frequency_correction: false` disables it.

## Timed transmit and bursts

Not every backend can schedule samples. Support is reported per capability:

```rust
use sdr_hal::TxCapability;

if tx.capabilities().supports(TxCapability::TimedSamples) {
    // schedule bursts
}
```

`TimedSamples`, `TimedActivation`, `ClockReset` and `BurstEnd` are reported
independently. A request for timed TX on a backend without support returns an
error, including through the network client.

`write` returns the number of samples the driver accepted. `write_all` writes
the whole buffer, advances the schedule by the applied sample rate between
chunks, and marks only the final samples as the end of the burst:

```rust
use sdr_hal::{DeviceTime, Schedule, TxMetadata};

tx.enable_transmit(true)?;
tx.write_all(&iq, TxMetadata {
    schedule: Schedule::At(DeviceTime::from_ticks(start_ticks)),
    end_of_burst: true,
})?;
```

An end-of-burst marker ends sample output and leaves the stream ready for the
next burst. `stop()` ends TX.

An error does not mean no samples were sent, and reports how many were. Success
does not mean the samples were transmitted, since they may still be buffered. No
backend reports when transmission finishes.

| Backend | End of burst |
| --- | --- |
| UHD, bladeRF | native burst flag |
| SoapySDR | the plugin's end-of-burst flag |
| LimeSuite | flushes the final partial packet, stream keeps running |

Some drivers accept burst metadata but reject a separate empty end marker. The
marker should be sent with the final samples where possible.

## Receiving

`RadioRx::read` takes `ReadTimeout::NonBlocking`, `For(Duration)` or `Forever`.
A backend rejects a wait mode it cannot implement. Each read reports the device
tick of its first sample, which follows the stream clock rather than wall time,
and whether samples were lost immediately before it.

## Test devices

Three backends need no radio:

- `MemoryRadio` (`memory`) exposes both ends of the IQ path, so a test can inject
  received samples and read back transmitted ones.
- `NoopRadio` produces silence and discards TX, paced at a set rate.
- `FileOutputRadio` (`file`) writes interleaved I/Q to a WAV at a fixed scale and
  rejects clipping. Its clock advances with sample position. Timed writes count
  nanoseconds from the start of the file, gaps become silence, overlap is an
  error, and fractional boundaries round to the nearest sample.
