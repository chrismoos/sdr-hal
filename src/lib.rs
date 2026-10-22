//! SDR drivers and streaming interfaces for complex IQ samples.
//!
//! Configure the sample rates and channels on a [`Radio`], then call [`Radio::split`]
//! to get separate TX and RX handles. Tune each handle before starting its stream.
//! You can move the handles to separate threads.
//!
//! Drivers can round the requested sample rate. Use the rate they return when
//! calculating sample durations. For timestamps, use [`RadioTx::tick_rate`] or
//! [`RadioRx::tick_rate`] instead: the device clock may run at a different rate.
//! Supply TX samples at the configured rate. Filtering and resampling are the
//! caller's job.
//!
//! No features are enabled by default. `file` and `memory` add software backends,
//! `dsp` adds FIR filters and an NCO, and `config` adds JSON and YAML configuration
//! and device opening. `uhd-backend`, `lime-backend`, `bladerf-backend`, and
//! `soapy-backend` require their native SDKs. `network-backend` adds a remote
//! client, `daemon` adds the server and `sdr-radiod`, and `tools` adds `sdr-bench`.
//!
//! ```
//! use num_complex::Complex32;
//! use sdr_hal::{NoopRadio, Radio};
//!
//! # fn main() -> Result<(), sdr_hal::error::Error> {
//! let mut radio = NoopRadio::new(2_000_000);
//! let applied_rate_hz = radio.set_tx_sample_rate(2_000_000.0)?;
//! assert_eq!(applied_rate_hz, 2_000_000.0);
//! let (mut tx, _) = Box::new(radio).split()?;
//! tx.set_tx_frequency_hz(100_000_000.0)?;
//! tx.enable_transmit(true)?;
//! let result = tx.transmit(&[Complex32::new(0.25, -0.25)]);
//! let stopped = tx.stop();
//! result?;
//! stopped?;
//! # Ok(())
//! # }
//! ```

//!
//! ## Submit a scheduled burst
//!
//! Burst metadata finishes sample output while keeping the stream ready. A successful
//! write means acceptance, so use device acknowledgements when RF completion matters.
//! Call `stop()` separately when finished with TX.
//!
//! ```
//! use num_complex::Complex32;
//! use sdr_hal::{DeviceTime, NoopRadio, Radio};
//! # fn main() -> Result<(), sdr_hal::error::Error> {
//! let (mut tx, _) = Box::new(NoopRadio::new(2_000_000)).split()?;
//! let iq = [Complex32::new(0.25, 0.0); 8];
//! let start = DeviceTime::from_ticks(tx.get_hardware_time()? + 2 * tx.tick_rate());
//! let submitted = tx.transmit_burst_at(&iq, start);
//! let stopped = tx.stop();
//! submitted?;
//! stopped?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Open a radio from a configuration file
//!
//! Enable `config` to load settings from JSON or YAML. This example uses the no-op
//! backend, so it
//! runs without hardware. It prepares both streams, then tunes and starts them.
//!
//! ```
//! # #[cfg(feature = "config")]
//! # fn main() -> Result<(), sdr_hal::error::Error> {
//! use num_complex::Complex32;
//! use sdr_hal::config::RadioConfig;
//!
//! let mut config: RadioConfig = serde_json::from_str(r#"{
//!     "kind": "noop",
//!     "tx_sample_rate_hz": 1000000,
//!     "tx_bandwidth_hz": 1500000,
//!     "rx_sample_rate_hz": 2000000,
//!     "rx_bandwidth_hz": 1500000
//! }"#)?;
//! config.tx_sample_rate_hz = 2_000_000;
//! let radio = config.open()?;
//! let actual_tx_rate_hz = radio.tx_sample_rate_hz();
//! let (mut tx, rx) = radio.split()?;
//! let mut rx = rx.ok_or("no RX stream")?;
//! tx.set_tx_frequency_hz(915_000_000.0)?;
//! rx.set_rx_frequency(915_000_000.0)?;
//! rx.rx_activate(None)?;
//! tx.enable_transmit(true)?;
//! let sent = tx.transmit(&[Complex32::new(0.25, -0.25)]);
//! let tx_stopped = tx.stop();
//! let rx_stopped = rx.stop();
//! sent?;
//! tx_stopped?;
//! rx_stopped?;
//! # Ok(())
//! # }
//! # #[cfg(not(feature = "config"))]
//! # fn main() {}
//! ```
//!
//! To load a file, replace `serde_json::from_str(...)` with
//! `RadioConfig::load("radio.json")` or `RadioConfig::load("radio.yaml")`. The
//! extension selects the format. A sibling `radio.local.json` or `radio.local.yaml`
//! can override settings without editing the base file.
//!
//! For a USRP, enable `uhd-backend` as well and use a configuration such as:
//!
//! ```json
//! {
//!   "kind": "uhd",
//!   "device": "type=b200",
//!   "channel": 0,
//!   "antenna": "TX/RX",
//!   "tx_gain_db": 40.0,
//!   "master_clock_rate": 32000000,
//!   "clock_source": "internal",
//!   "time_source": "internal",
//!   "rx_antenna": "RX2",
//!   "rx_gain_db": 30.0,
//!   "tx_max_gain_db": 60.0,
//!   "tx_max_gain_power_estimate_dbm": 0.0,
//!   "rx_reference_dbm": -30.0,
//!   "rx_to_tx_delay_ns": 0,
//!   "tx_sample_rate_hz": 2000000,
//!   "tx_bandwidth_hz": 1500000,
//!   "rx_sample_rate_hz": 2000000,
//!   "rx_bandwidth_hz": 1500000
//! }
//! ```
//!
//! The calibration numbers above are illustrative. Replace them with measurements
//! for your radio, or omit them to leave calibration unknown. Opening attaches
//! these values to the radio and its split handles. It adjusts the TX power estimate
//! for `tx_gain_db` relative to `tx_max_gain_db`. Frequency is set in code after splitting.

#![deny(missing_docs, rustdoc::broken_intra_doc_links)]

mod calibration;
#[cfg(any(feature = "config", feature = "uhd-backend"))]
mod clock;
/// Shared error type for driver, I/O, and unsupported-operation failures.
pub mod error;
pub use calibration::{CalibratedRadio, RadioCalibration};
mod radio;
mod stream;
pub use radio::{Radio, RadioClock, RadioRx, RadioTx, RxReadResult, TxRadioHealth};
pub use stream::{DeviceTime, ReadTimeout, Schedule, TxCapabilities, TxCapability, TxMetadata};
mod noop;
pub use noop::NoopRadio;
#[cfg(feature = "file")]
mod file;
#[cfg(feature = "file")]
pub use file::FileOutputRadio;
/// Radio settings and calibration, loaded from JSON or YAML. Available with `config`.
#[cfg(feature = "config")]
pub mod config;
#[cfg(feature = "config")]
mod config_load;
/// Remote control service and IQ bridges. Requires `daemon`.
#[cfg(feature = "daemon")]
pub mod daemon;
/// FIR filters and frequency shifting. Available with `dsp`.
#[cfg(feature = "dsp")]
pub mod dsp;
/// Versioned IQ framing, with client and clock support under `network-backend`.
pub mod network;
#[cfg(feature = "config")]
mod open;
/// USRP backend. Requires `uhd-backend` and the UHD native library.
#[cfg(feature = "uhd-backend")]
mod uhd_radio;
#[cfg(feature = "uhd-backend")]
pub use uhd_radio::UhdRadio;
/// LimeSDR backend. Requires `lime-backend` and the LimeSuite native library.
#[cfg(feature = "lime-backend")]
mod lime_radio;
#[cfg(feature = "lime-backend")]
pub use lime_radio::LimeRadio;
/// bladeRF backend. Requires `bladerf-backend` and the libbladeRF native library.
#[cfg(feature = "bladerf-backend")]
mod bladerf_radio;
#[cfg(feature = "bladerf-backend")]
pub use bladerf_radio::BladeRfRadio;
/// SoapySDR backend. Requires `soapy-backend` and installed device modules.
#[cfg(feature = "soapy-backend")]
mod soapy_radio;
#[cfg(feature = "network-backend")]
pub use network::{NetworkRadio, NetworkRadioOptions};
#[cfg(feature = "soapy-backend")]
pub use soapy_radio::SoapySdrRadio;

/// Bounded IQ channels with a nanosecond clock. Requires `memory`.
#[cfg(feature = "memory")]
pub mod memory;
