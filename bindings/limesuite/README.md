# sdr-hal-limesuite

Safe Rust bindings to the LimeSuite library for LimeSDR devices. This crate
backs the `lime-backend` feature of [`sdr-hal`](https://crates.io/crates/sdr-hal)
and can be used on its own.

It covers device open, tuning, gain, sample rate, calibration and timestamped
TX and RX streams.

## Requirements

The LimeSuite native library must be installed and findable by pkg-config. On
Debian and Ubuntu that is `liblimesuite-dev`.

## License

MIT OR Apache-2.0, at your option.

LimeSuite itself is Apache-2.0. The license of these Rust bindings does not
change the license of the native library.
