# sdr-hal

[![CI](https://github.com/chrismoos/sdr-hal/actions/workflows/ci.yml/badge.svg)](https://github.com/chrismoos/sdr-hal/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/sdr-hal.svg)](https://crates.io/crates/sdr-hal)
[![docs.rs](https://img.shields.io/docsrs/sdr-hal)](https://docs.rs/sdr-hal)
![Rust 1.96+](https://img.shields.io/badge/rust-1.96%2B-orange.svg)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

sdr-hal streams complex IQ samples to and from an SDR, local or across the network, through one Rust interface.

Hardware drivers, DSP helpers, test devices and a radio daemon are optional features of this crate. No feature is enabled by default.

```toml
[dependencies]
sdr-hal = { version = "0.1.0", features = ["uhd-backend", "config"] }
```

## Quick start

An application sets the rates on the device, splits it into a transmit half and a receive half, tunes, and then writes samples:

```rust
use num_complex::Complex32;
use sdr_hal::{NoopRadio, Radio};

let mut radio = NoopRadio::new(2_000_000);
let applied_rate_hz = radio.set_tx_sample_rate(2_000_000.0)?;

let (mut tx, _rx) = Box::new(radio).split()?;
tx.set_tx_frequency_hz(100_000_000.0)?;
tx.enable_transmit(true)?;
tx.transmit(&[Complex32::new(0.25, -0.25); 1024])?;
tx.stop()?;
```

`UhdRadio`, `LimeRadio`, `BladeRfRadio`, `SoapySdrRadio` and `NetworkRadio` can replace `NoopRadio` with no other change. The following example runs the full path without hardware:

```sh
cargo run --release --example iq_roundtrip --features memory
```

Streaming code must be built with `--release`. Debug builds cannot keep up with the sample rate.

## Features

| Feature | Includes | Native requirement |
| --- | --- | --- |
| `uhd-backend` | USRP driver | UHD |
| `lime-backend` | LimeSDR driver | LimeSuite |
| `bladerf-backend` | bladeRF driver | libbladeRF |
| `soapy-backend` | SoapySDR adapter | SoapySDR and device plugin |
| `network-backend` | Remote IQ client and generated control protocol | None |
| `config` | JSON and YAML configuration, and device opening | None |
| `dsp` | FIR filters, interpolation and frequency-shifting NCO | None |
| `file` | IQ WAV output | None |
| `memory` | In-memory IQ radio and injection handle | None |
| `daemon` | Remote radio server and `sdr-radiod` binary | Selected driver |
| `tools` | Bounded `sdr-bench` streaming tool | Selected driver |

The native bindings are separate packages under `bindings/`, each with its own library link, build script and license. Applications depend on `sdr-hal` only.

## Interface

A `Radio` is the device before streaming starts. Rates, channels, antennas and bandwidth are set on it. `split()` consumes it and returns a `RadioTx` and an optional `RadioRx`. The two halves are independent and can run on separate threads.

Frequency is set after the split, with `RadioTx::set_tx_frequency_hz` and `RadioRx::set_rx_frequency`. Opening a radio does not set a frequency, and neither does preparing a stream. After `setup_rx`, the RX stream stays inactive until it is activated.

The waveform belongs to the application. The crate reads and writes `Complex32` samples at the final rate and never filters, resamples or scales them. Full scale is ±1.0 on each component.

## Sample rates

Drivers apply the requested rate when the hardware supports it. Some round it to the nearest supported rate, so the application should use the applied rate that each setter returns.

- `set_tx_sample_rate` takes Hz as `f64` and returns the applied rate. bladeRF rounds a fractional rate to whole Hz. File output rejects one.
- `setup_rx` returns the rate the hardware applied.
- `tick_rate()` returns the clock that timestamps count in, which can differ from the sample rate.

When a radio is opened from a configuration file, `tx_rate_policy` and `rx_rate_policy` set how far the applied rate may differ from the request:

| Policy | Opening fails unless |
| --- | --- |
| `accept` | never, the applied rate is taken as is |
| `exact` | the applied rate matches the request exactly |
| `nearest_whole_hz` | it matches after rounding to whole Hz |
| `nominal` | it differs by no more than 1 ppm or 1 Hz, whichever is larger. The default for both |

Frequencies, rates and bandwidths must be set by the application. The exception is the USRP master clock. When `master_clock_rate` is omitted, the largest supported clock that every configured rate divides evenly is selected, so each sample spans a whole number of ticks. When it is set, it is used as given, and a rate that does not divide it evenly is an error at open time.

For UHD, software TX frequency correction is enabled by default to work around a UHD bug that causes unwanted phase jumps when late samples interrupt TX playback. Set `software_tx_frequency_correction: false` to disable it.

## Timed transmit and bursts

Not every backend can schedule samples. Support is reported per capability:

```rust
use sdr_hal::TxCapability;

if tx.capabilities().supports(TxCapability::TimedSamples) {
    // schedule bursts
}
```

`TimedSamples`, `TimedActivation`, `ClockReset` and `BurstEnd` are reported independently. A request for timed TX on a backend without support returns an error, including through the network client.

`write` returns the number of samples the driver accepted. `write_all` writes the whole buffer, advances the schedule by the applied sample rate between chunks, and marks only the final samples as the end of the burst:

```rust
use sdr_hal::{DeviceTime, Schedule, TxMetadata};

tx.enable_transmit(true)?;
tx.write_all(&iq, TxMetadata {
    schedule: Schedule::At(DeviceTime::from_ticks(start_ticks)),
    end_of_burst: true,
})?;
```

An end-of-burst marker ends sample output and leaves the stream ready for the next burst. `stop()` ends TX.

An error does not mean no samples were sent. The error reports a progress count. Success does not mean the samples were transmitted either, since they may still be buffered, and no backend reports when transmission finishes. End of burst differs by backend:

| Backend | End of burst |
| --- | --- |
| UHD, bladeRF | native burst flag |
| SoapySDR | the plugin's end-of-burst flag |
| LimeSuite | flushes the final partial packet, stream keeps running |

Some drivers accept burst metadata but reject a separate empty end marker. The marker should be sent with the final samples where possible.

## Receiving

`RadioRx::read` takes `ReadTimeout::NonBlocking`, `For(Duration)` or `Forever`. A backend rejects a wait mode it cannot implement. Each read reports the device tick of its first sample, which follows the stream clock rather than wall time, and whether samples were lost immediately before it.

Error handling should branch on `Error::kind()`, not on message text.

## Configure from a file

With the `config` feature, settings load from a file, can be changed in code, and then open the device:

```rust
use sdr_hal::config::RadioConfig;

let mut config = RadioConfig::load("config/radio_uhd_b210.json")?;
config.tx_sample_rate_hz = 4_000_000;
let radio = config.open()?;
```

The file extension selects the parser, `.json` or `.yaml` / `.yml`. The `kind` field selects the backend, and the backend's fields sit at the top level next to the stream settings:

```json
{
  "kind": "uhd",
  "device": "type=b200",
  "channel": 0,
  "antenna": "TX/RX",
  "tx_gain_db": 60,
  "master_clock_rate": 32000000,
  "tx_sample_rate_hz": 2000000,
  "tx_bandwidth_hz": 1500000,
  "rx_antenna": "RX2",
  "rx_gain_db": 30,
  "rx_sample_rate_hz": 2000000,
  "rx_bandwidth_hz": 1500000
}
```

The same settings in YAML:

```yaml
kind: uhd
device: type=b200
channel: 0
antenna: TX/RX
tx_gain_db: 60
master_clock_rate: 32000000
tx_sample_rate_hz: 2000000
tx_bandwidth_hz: 1500000
rx_antenna: RX2
rx_gain_db: 30
rx_sample_rate_hz: 2000000
rx_bandwidth_hz: 1500000
```

Without both RX stream fields, the radio is TX only. A TX rate is required before opening. Unknown fields are rejected, not ignored.

A local override with the `<name>.local.<ext>` pattern, next to the base file, overrides it without editing it, so machine-specific device selectors and gains can stay out of version control. The override uses the base file's format, objects merge key by key, and `null` removes a key. `config/` has a starting point for every backend:

| File | Backend |
| --- | --- |
| `radio_uhd_b210.json` | `uhd`, USRP B210 |
| `radio_limesdr_mini.json` | `lime`, LimeSDR Mini |
| `radio_bladerf_micro.json` | `blade_rf`, bladeRF 2.0 micro with its transfer buffers |
| `radio_soapy_plutosdr.json` | `soapy`, PlutoSDR through SoapySDR |
| `radio_network.json` | `network`, client for a remote `sdr-radiod` |
| `radio_file.json` | `file_output`, IQ WAV, TX only |
| `radio_noop.json` | `noop`, paced silence, no hardware |

The gains, ports and power references in those files suit those models but are not calibration. `tx_max_gain_power_estimate_dbm` and `rx_max_gain_reference_dbm` should be measured on each unit before they are relied on.

## Test devices

Three backends need no radio:

- `MemoryRadio` (`memory`) exposes both ends of the IQ path, so a test can inject received samples and read back transmitted ones.
- `NoopRadio` produces silence and discards TX, paced at a set rate.
- `FileOutputRadio` (`file`) writes interleaved I/Q to a WAV at a fixed scale and rejects clipping. Its clock advances with sample position, timed writes count nanoseconds from the start of the file, gaps become silence, overlap is an error, and fractional boundaries round to the nearest sample.

## Remote radio

`sdr-radiod` shares a local SDR over the network. A client opens it with the `network` device kind and uses it through the same interface as a local radio.

The daemon is built with the backend it drives and started with a hardware file and a listener file:

```sh
cargo build --release --features daemon,uhd-backend --bin sdr-radiod
sdr-radiod --radio-config config/radio_uhd_b210.json --config config/radiod_network.json
```

- **The hardware file** selects the device: `noop`, `file_output`, `uhd`, `lime`, `blade_rf` or `soapy`. It cannot be another `network` radio.
- **The listener file** holds only the control and data addresses, 50710 and 50711 by default.
- **Rates are set by the client.** Outside composite mode, the daemon ignores any rate in the hardware file.

A client configures a `network` radio with the daemon's address:

```json
{
  "kind": "network",
  "addr": "radio-host:50710",
  "transport": "tcp"
}
```

`transport` selects how IQ samples are sent, for TX and RX alike. The daemon accepts both, so clients can use different transports:

| Transport | Suited to | On a lost packet |
| --- | --- | --- |
| `udp` (default) | client and server share a host or a clean LAN | missing TX becomes silence, RX shows a gap |
| `tcp` | the path drops packets, such as a VM's user-mode network | nothing is lost |

Client and server must be the same version. There is one wire version and no negotiation.

- **Timestamps are the server device's ticks**, and `tick_rate()` reports that clock. The server selects a clock that fits the client's rates. `tick_rate` should be set only when a specific clock is required, and before any sample rate.
- **`get_hardware_time()` reads slightly behind the device.** It is suitable for choosing a TX lead, not for an exact tick. RX timestamps come from the device itself.
- **Frequency is set on each half after `split()`.** RX setup takes no frequency.
- **TX stops when samples stop arriving**, and a graceful shutdown stops TX and RX before exiting. This is the host's own report, not proof that the radio stopped radiating.
- **Calibration values** (`rx_to_tx_delay_ns`, `tx_full_scale_power_estimate_dbm`, `rx_reference_dbm`) from the hardware file are passed to clients. Configured values take precedence over the backend's own. The RX reference is unavailable while RX gain differs from the gain it was measured at.

## Several clients on one device

Composite mode lets several clients share one device, each on its own carrier and sample rate. Clients need no changes.

Composite mode is enabled by a `composite` block in the listener file:

```json
{
  "listen": "0.0.0.0:50710",
  "data_listen": "0.0.0.0:50711",
  "composite": {
    "device_rate_hz": 9720000,
    "tx_center_hz": 864945000,
    "rx_center_hz": 819945000,
    "tx_digital_backoff_db": -12.0
  }
}
```

| Setting | Meaning |
| --- | --- |
| `device_rate_hz` | width of the shared stream |
| `tx_center_hz`, `rx_center_hz` | device tuning, midway between the client carriers |
| `tx_digital_backoff_db` | headroom per client. Use about -10 for three carriers |
| `block_duration_ms` | how much of the stream the daemon mixes at once, default 2 ms. RX latency is about one block |
| `tx_deadline_ms` | when a block is committed, default 5 ms |

- **Fit:** each carrier's offset from center plus half its sample rate must be at most half of `device_rate_hz`.
- **Rates:** any fraction of `device_rate_hz` up to 4096/4096, in any combination and connection order.
- **TX lead:** TX must be scheduled at least 20 ms ahead.
- **Gain, bandwidth and LO offset** belong to the server. Client requests are logged and ignored.
- **A client that leaves** frees its slot. When the last one goes quiet, the device stops transmitting.

## SoapySDR applications

The `soapy/` module lets GNU Radio, SDR++, SDRangel and other SoapySDR applications use an
`sdr-radiod` server as a local device. Building it requires SoapySDR's development files,
CMake and Cargo:

```sh
cmake -S soapy -B build/soapy && cmake --build build/soapy
sudo cmake --install build/soapy
```

On macOS with Homebrew, the module must be installed into Homebrew's prefix for its SoapySDR to find it:

```sh
brew install soapysdr cmake
cmake -S soapy -B build/soapy && cmake --build build/soapy
cmake --install build/soapy --prefix "$(brew --prefix)"
```

The device is opened by address:

```sh
SoapySDRUtil --probe="driver=sdrhal,addr=radio-host:50710"
```

| Argument | Meaning |
| --- | --- |
| `addr` | server address. Port 50710 when omitted |
| `transport` | `tcp` (default) or `udp` |
| `hardware` | board name the driver reports, such as `B210` for applications that check it. The server's backend when omitted |

Against a composite server, applications offer only the rates and frequencies the plan
accepts. A single-client server lets the application set TX gain. A composite server maintains
its configured gain.

> Stream settings are locked while streaming. The stream must be stopped to change the sample
> rate. A single-client server keeps its first rate until it restarts. A composite server
> does not.

## Streaming benchmark

`sdr-bench` measures whether a host and driver sustain a sample rate without underrunning. Every parameter is explicit.

```sh
cargo run --release --features tools --bin sdr-bench -- \
  --radio-config config/radio_noop.json \
  --frequency-hz 100000000 --sample-rate-hz 2000000 \
  --bandwidth-hz 2000000 --batch-samples 2048 --duration-seconds 1
```

It prints the applied rate, samples submitted, elapsed time and the driver's underflow, late-packet and dropped-packet counters. TX stops at the duration limit, on error, or on Ctrl-C. The payload is a constant, so on real hardware it transmits an unmodulated carrier at the tuned frequency. `config/radio_noop.json` runs the full path with no RF.

## Development

Rust 1.96 is the minimum and is pinned for development. Native drivers require their SDKs to be visible to `pkg-config`, and libclang for the generated FFI.

```sh
cargo fmt --all -- --check
cargo build --release --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --release --workspace --all-features
cargo test --release -p sdr-hal --all-features --doc
cargo doc -p sdr-hal --all-features --no-deps
```

When a native SDK is missing, `--all-features` should be replaced with the features that are available. Rustdoc output goes to `target/doc/sdr_hal/index.html`. Undocumented public exports and broken intra-doc links fail the build.

A backend must keep device ownership exclusive. Settings and streaming can share a device, but TX clock reads and sample writes must never wait on a settings lock.

## License

`sdr-hal` is Apache-2.0 ([LICENSE](LICENSE)). The UHD, LimeSuite and bladeRF binding packages are MIT OR Apache-2.0 ([LICENSE-MIT](LICENSE-MIT)).

Native SDK licensing is separate from the Rust bindings. The UHD library is mostly GPLv3, which can affect how it may be linked.
