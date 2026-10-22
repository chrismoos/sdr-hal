//! C interface over [`Device`], for the SoapySDR module built from `soapy/module`.
//!
//! The module's C++ code owns SoapySDR registration and the stream API. Everything that
//! talks to the server, including the session lifecycle, lives on this side.

mod device;
mod ffi;

pub use device::{Device, Direction, ReadOutcome, TxEvent, TxTiming, WriteOutcome};
