# sdr-hal

[![CI](https://github.com/chrismoos/sdr-hal/actions/workflows/ci.yml/badge.svg)](https://github.com/chrismoos/sdr-hal/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/sdr-hal.svg)](https://crates.io/crates/sdr-hal)
[![docs.rs](https://img.shields.io/docsrs/sdr-hal)](https://docs.rs/sdr-hal)
![Rust 1.96+](https://img.shields.io/badge/rust-1.96%2B-orange.svg)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

sdr-hal streams complex IQ samples to and from an SDR, local or across the
network, through one Rust interface. It includes drivers for USRP, LimeSDR,
bladeRF and SoapySDR devices, test devices that need no hardware, and
`sdr-radiod`, a daemon that shares one SDR with several clients over the
network.

Transmitting requires authorization for the frequencies in use.

## Documentation

- [Interface](docs/interface.md): splitting a radio, sample rates, timed
  transmit, bursts and receiving.
- [Configuration](docs/configuration.md): opening a device from a JSON or YAML
  file, and the example file for each backend.
- [Remote radio](docs/radiod.md): sharing a device over the network with
  `sdr-radiod`.
- [Composite mode](docs/composite.md): several clients on one device, and
  `sdr-radiod plan` for choosing its settings.
- [SoapySDR module](docs/soapy.md): using an `sdr-radiod` server from GNU Radio,
  SDR++ and other SoapySDR applications.
- [Streaming benchmark](docs/sdr-bench.md): checking that a host and driver
  sustain a sample rate.

API documentation is on [docs.rs](https://docs.rs/sdr-hal).

## Quick start

```toml
[dependencies]
sdr-hal = { version = "0.1", features = ["uhd-backend", "config"] }
```

An application sets the rates on the device, splits it into a transmit half and
a receive half, tunes, and then writes samples:

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

`UhdRadio`, `LimeRadio`, `BladeRfRadio`, `SoapySdrRadio` and `NetworkRadio`
implement the same interface as `NoopRadio`. The full path runs without
hardware in this example:

```sh
cargo run --release --example iq_roundtrip --features memory
```

Streaming code must be built with `--release`. Debug builds cannot keep up with
the sample rate.

## Features

No feature is enabled by default.

| Feature | Includes | Native requirement |
| --- | --- | --- |
| `uhd-backend` | USRP driver | UHD |
| `lime-backend` | LimeSDR driver | LimeSuite |
| `bladerf-backend` | bladeRF driver | libbladeRF |
| `soapy-backend` | SoapySDR adapter | SoapySDR and device plugin |
| `network-backend` | Remote IQ client and control protocol | None |
| `config` | JSON and YAML configuration, and device opening | None |
| `dsp` | FIR filters, interpolation and frequency-shifting NCO | None |
| `file` | IQ WAV output | None |
| `memory` | In-memory IQ radio and injection handle | None |
| `daemon` | Remote radio server and `sdr-radiod` binary | Selected driver |
| `tools` | `sdr-bench` streaming tool | Selected driver |

The native bindings are separate packages under `bindings/`, each with its own
library link, build script and license. Applications depend on `sdr-hal` only.

## Development

Rust 1.96 is the minimum and is pinned for development. Native drivers require
their SDKs to be visible to `pkg-config`, and libclang for the generated FFI.

```sh
cargo fmt --all -- --check
cargo build --release --workspace --all-targets --all-features
cargo clippy --release --workspace --all-targets --all-features -- -D warnings
cargo test --release --workspace --all-features
cargo test --release -p sdr-hal --all-features --doc
cargo doc -p sdr-hal --all-features --no-deps
```

When a native SDK is missing, `--all-features` must be replaced with the
features that are available. Undocumented public exports and broken intra-doc
links fail the build.

A backend must keep device ownership exclusive. Settings and streaming can share
a device, but TX clock reads and sample writes must never wait on a settings
lock.

## License

`sdr-hal` is Apache-2.0 ([LICENSE](LICENSE)). The UHD, LimeSuite and bladeRF
binding packages are MIT OR Apache-2.0 ([LICENSE-MIT](LICENSE-MIT)).

Native SDK licensing is separate from the Rust bindings. The UHD library is
mostly GPLv3, which can affect how it may be linked.
