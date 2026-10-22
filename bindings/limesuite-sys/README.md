# sdr-hal-limesuite-sys

Low-level FFI bindings to the LimeSuite library for LimeSDR devices, generated
with bindgen. Used by [`sdr-hal-limesuite`](https://crates.io/crates/sdr-hal-limesuite).

## Requirements

The LimeSuite native library must be installed and findable by pkg-config, and
bindgen needs libclang. On Debian and Ubuntu those are `liblimesuite-dev` and
`libclang-dev`.

## License

MIT OR Apache-2.0, at your option.

LimeSuite itself is Apache-2.0. The license of these Rust bindings does not
change the license of the native library.
