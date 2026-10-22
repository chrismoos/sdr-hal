# sdr-hal-uhd

Safe Rust bindings to the UHD library for USRP devices. This crate backs the
`uhd-backend` feature of [`sdr-hal`](https://crates.io/crates/sdr-hal) and can
be used on its own.

## Requirements

The UHD native library must be installed and findable by pkg-config. On Debian
and Ubuntu that is `libuhd-dev`.

## License

MIT OR Apache-2.0, at your option. Adapted from
[uhd-rust](https://github.com/samcrow/uhd-rust) by Sam Crow.

The UHD library itself is mostly GPLv3. Depending on how you link it, that may
have licensing implications for your binary. The license of these Rust bindings
does not change the license of the native library.
