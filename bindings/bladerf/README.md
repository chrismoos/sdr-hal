# sdr-hal-bladerf

Safe Rust bindings to the libbladeRF library for bladeRF devices. This crate
backs the `bladerf-backend` feature of [`sdr-hal`](https://crates.io/crates/sdr-hal)
and can be used on its own.

It covers device open, tuning, gain, sample rate, RF port selection and
timestamped TX and RX streams through the synchronous interface.

## Requirements

The libbladeRF native library must be installed and findable by pkg-config. On
Debian and Ubuntu that is `libbladerf-dev`.

## License

MIT OR Apache-2.0, at your option.

libbladeRF is licensed separately, mostly under the LGPL. The license of these
Rust bindings does not change the license of the native library.
