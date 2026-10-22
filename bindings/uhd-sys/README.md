# sdr-hal-uhd-sys

Low-level FFI bindings to the UHD library for USRP devices, generated with
bindgen. Used by [`sdr-hal-uhd`](https://crates.io/crates/sdr-hal-uhd).

## Requirements

The UHD native library must be installed and findable by pkg-config, and
bindgen needs libclang. On Debian and Ubuntu those are `libuhd-dev` and
`libclang-dev`.

## License

MIT OR Apache-2.0, at your option. Adapted from
[uhd-rust](https://github.com/samcrow/uhd-rust) by Sam Crow.

The UHD library itself is mostly GPLv3. Depending on how you link it, that may
have licensing implications for your binary. The license of these Rust bindings
does not change the license of the native library.
