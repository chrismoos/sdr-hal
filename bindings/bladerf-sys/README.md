# sdr-hal-bladerf-sys

Low-level FFI bindings to the libbladeRF library for bladeRF devices, generated
with bindgen. Used by [`sdr-hal-bladerf`](https://crates.io/crates/sdr-hal-bladerf).

## Requirements

The libbladeRF native library must be installed and findable by pkg-config, and
bindgen needs libclang. On Debian and Ubuntu those are `libbladerf-dev` and
`libclang-dev`.

## License

MIT OR Apache-2.0, at your option.

libbladeRF is licensed separately, mostly under the LGPL. The license of these
Rust bindings does not change the license of the native library.
