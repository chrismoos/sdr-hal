mod validated;
pub(crate) use validated::Settings;

use crate::error::Error;
use serde::{Deserialize, Serialize};
use std::path::Path;
fn default_tx_gain_db() -> f64 {
    60.0
}

fn default_lime_tx_antenna() -> String {
    "BAND1".to_string()
}

fn default_lime_tx_gain_db() -> u32 {
    60
}

fn default_bladerf_tx_gain_db() -> i32 {
    60
}

fn default_bladerf_tx_antenna() -> Option<String> {
    Some("TXA".to_string())
}

fn default_bladerf_rx_antenna() -> Option<String> {
    Some("B_BALANCED".to_string())
}

fn default_network_samples_per_packet() -> usize {
    crate::network::wire::DEFAULT_SAMPLES_PER_PACKET
}

fn default_true() -> bool {
    true
}

/// Radio configuration. The `kind` field selects the backend.
///
/// Unknown fields are rejected. The default backend is `noop`. You can load
/// a configuration without its driver feature, but opening the radio requires it.
/// Calibration describes power and timing. It does not change hardware settings.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum DeviceConfig {
    /// `file_output`: stereo IQ WAV. Requires `file` when constructed.
    FileOutput {
        /// Output filename. Construction creates or truncates the file.
        path: String,
    },
    /// `noop`: paced TX sink and optional silent RX.
    Noop {},
    /// `soapy`: SoapySDR device arguments. Requires `soapy-backend`.
    Soapy {
        /// Device selector passed to the driver. Use the selector syntax for that driver.
        device: String,
        /// Zero-based hardware channel index.
        channel: usize,
        /// TX antenna or port name accepted by the driver.
        antenna: String,
        /// TX hardware gain in dB. Defaults to 60.
        #[serde(default = "default_tx_gain_db")]
        tx_gain_db: f64,
        /// RX port override. `None` uses LNAW.
        #[serde(default)]
        rx_antenna: Option<String>,
        /// RX gain in dB. Leave unset to keep the driver gain or the remote server default.
        #[serde(default)]
        rx_gain_db: Option<f64>,
        /// Input power in dBm corresponding to 0 dBFS at the configured RX gain. `None` means uncalibrated.
        #[serde(default)]
        rx_reference_dbm: Option<f64>,
        /// RX-to-TX timestamp correction in nanoseconds. Add it once in the scheduler.
        /// See [`crate::RadioCalibration::rx_to_tx_delay_ns`] for the sign and clock requirements.
        #[serde(default)]
        rx_to_tx_delay_ns: Option<i64>,
    },
    /// `uhd`: USRP device arguments. Requires `uhd-backend`.
    Uhd {
        /// Device selector passed to the driver. Use the selector syntax for that driver.
        device: String,
        /// Requires timestamped TX and zero intentional LO offset. RF rounding is
        /// corrected in software so hardware playback restarts do not reset its phase.
        #[serde(default = "default_true")]
        software_tx_frequency_correction: bool,
        /// Zero-based hardware channel index.
        channel: usize,
        /// TX antenna or port name accepted by the driver.
        antenna: String,
        /// TX hardware gain in dB. Defaults to 60.
        #[serde(default = "default_tx_gain_db")]
        tx_gain_db: f64,
        /// UHD TX gain in dB where the unit-RMS connector power estimate applies.
        #[serde(default)]
        tx_max_gain_db: Option<f64>,
        /// Approximate unit-RMS connector power in dBm at `tx_max_gain_db`.
        #[serde(default)]
        tx_max_gain_power_estimate_dbm: Option<f32>,
        /// Requested UHD master-clock frequency in Hz. Absent selects the largest device
        /// clock that is a whole multiple of every configured sample rate. Runtime tick
        /// rate uses the applied clock.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        master_clock_rate: Option<u64>,
        /// Highest master clock in Hz that automatic selection may request. Absent
        /// discovers a ceiling from the device, which costs a few clock writes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_master_clock_rate: Option<u64>,
        /// UHD reference-clock source, such as `internal` or `external`. `None` leaves the driver setting unchanged.
        #[serde(default)]
        clock_source: Option<String>,
        /// UHD time-reference source. `None` leaves the driver setting unchanged.
        #[serde(default)]
        time_source: Option<String>,
        /// RX port override. `None` uses RX2.
        #[serde(default)]
        rx_antenna: Option<String>,
        /// RX gain in dB. Leave unset to keep the driver gain or the remote server default.
        #[serde(default)]
        rx_gain_db: Option<f64>,
        /// Input power in dBm corresponding to 0 dBFS at the configured RX gain. `None` means uncalibrated.
        #[serde(default)]
        rx_reference_dbm: Option<f64>,
        /// RX gain in dB at which `rx_max_gain_reference_dbm` was measured.
        #[serde(default)]
        rx_max_gain_db: Option<f64>,
        /// Approximate input power in dBm at 0 dBFS when RX gain is `rx_max_gain_db`.
        #[serde(default)]
        rx_max_gain_reference_dbm: Option<f64>,
        /// RX-to-TX timestamp correction in nanoseconds. Add it once in the scheduler.
        /// See [`crate::RadioCalibration::rx_to_tx_delay_ns`] for the sign and clock requirements.
        #[serde(default)]
        rx_to_tx_delay_ns: Option<i64>,
    },
    /// `lime`: LimeSuite device selection. Requires `lime-backend`.
    Lime {
        /// Device selector passed to the driver. Use the selector syntax for that driver.
        device: String,
        /// Zero-based hardware channel index.
        channel: usize,
        /// Native TX port name. Defaults to BAND1.
        #[serde(default = "default_lime_tx_antenna")]
        tx_antenna: String,
        /// TX hardware gain in dB. Defaults to 60.
        #[serde(default = "default_lime_tx_gain_db")]
        tx_gain_db: u32,
        /// RX port override. `None` uses LNAW.
        #[serde(default)]
        rx_antenna: Option<String>,
        /// RX gain in dB. Leave unset to keep the driver gain or the remote server default.
        #[serde(default)]
        rx_gain_db: Option<u32>,
        /// Input power in dBm corresponding to 0 dBFS at the configured RX gain. `None` means uncalibrated.
        #[serde(default)]
        rx_reference_dbm: Option<f64>,
        /// RX-to-TX timestamp correction in nanoseconds. Add it once in the scheduler.
        /// See [`crate::RadioCalibration::rx_to_tx_delay_ns`] for the sign and clock requirements.
        #[serde(default)]
        rx_to_tx_delay_ns: Option<i64>,
        /// LimeSuite hardware oversampling factor. `None` passes zero for driver selection.
        #[serde(default)]
        oversample: Option<usize>,
        /// TX LO offset in Hz. Digital frequency shifting keeps the transmitted signal at the requested center.
        #[serde(default)]
        tx_lo_offset_hz: Option<i64>,
        /// TX FIFO capacity in samples. `None` uses 1,048,576.
        #[serde(default)]
        tx_fifo_size: Option<u32>,
        /// RX FIFO capacity in samples. `None` uses 1,048,576.
        #[serde(default)]
        rx_fifo_size: Option<u32>,
        /// LimeSuite stream preference from 0 (latency) to 1 (throughput). `None` uses 0.
        #[serde(default)]
        stream_throughput_vs_latency: Option<f32>,
    },
    /// `blade_rf`: libbladeRF device selection. Requires `bladerf-backend`.
    BladeRf {
        /// Device selector passed to the driver. Use the selector syntax for that driver.
        #[serde(default)]
        device: String,
        /// Zero-based hardware channel index.
        #[serde(default)]
        channel: u32,
        /// Optional bladeRF FPGA image loaded only if the FPGA is not configured.
        #[serde(default)]
        fpga_path: Option<String>,
        /// Native TX port name. An omitted field defaults to TXA. `None` leaves the port unchanged.
        #[serde(default = "default_bladerf_tx_antenna")]
        tx_antenna: Option<String>,
        /// RX port override. An omitted field defaults to B_BALANCED. Explicit `None` leaves the driver port unchanged.
        #[serde(default = "default_bladerf_rx_antenna")]
        rx_antenna: Option<String>,
        /// TX hardware gain in dB. Defaults to 60.
        #[serde(default = "default_bladerf_tx_gain_db")]
        tx_gain_db: i32,
        /// RX gain in dB. Leave unset to keep the driver gain or the remote server default.
        #[serde(default)]
        rx_gain_db: Option<i32>,
        /// Input power in dBm corresponding to 0 dBFS at the configured RX gain. `None` means uncalibrated.
        #[serde(default)]
        rx_reference_dbm: Option<f64>,
        /// RX-to-TX timestamp correction in nanoseconds. Add it once in the scheduler.
        /// See [`crate::RadioCalibration::rx_to_tx_delay_ns`] for the sign and clock requirements.
        #[serde(default)]
        rx_to_tx_delay_ns: Option<i64>,
        /// TX LO offset in Hz. Digital frequency shifting keeps the transmitted signal at the requested center.
        #[serde(default)]
        tx_lo_offset_hz: Option<i64>,
        /// bladeRF transfer buffer count. `None` uses 16.
        #[serde(default)]
        num_buffers: Option<u32>,
        /// bladeRF buffer size in samples. `None` uses 8192. Must be a multiple of 1024.
        #[serde(default)]
        buffer_size: Option<u32>,
        /// bladeRF simultaneous transfer count. `None` uses 8 and must be less than buffer count.
        #[serde(default)]
        num_transfers: Option<u32>,
        /// bladeRF stream timeout in milliseconds. `None` uses 3500.
        #[serde(default)]
        stream_timeout_ms: Option<u32>,
    },
    /// `network`: remote control endpoint. Requires `network-backend`.
    Network {
        /// Control endpoint as `host:port`, without a URI scheme.
        addr: String,
        /// IQ host override. `None` uses the control endpoint host and the server-advertised port.
        #[serde(default)]
        data_host: Option<String>,
        /// Requested IQ samples per packet. Defaults to 350. The server caps the negotiated count.
        #[serde(default = "default_network_samples_per_packet")]
        samples_per_packet: usize,
        /// IQ transport for both TX and RX. Defaults to UDP.
        #[serde(default)]
        transport: crate::network::wire::Transport,
        /// Remote device clock in ticks per second, applied before any sample rate.
        /// Leave unset to keep the server's own clock. Backends whose clock follows the
        /// sample rate reject a change.
        #[serde(default)]
        tick_rate: Option<u64>,
        /// Allows RX for this configuration. Defaults to true. Opening RX also requires stream settings in [`RadioConfig::rx_sample_rate_hz`].
        #[serde(default = "default_true")]
        rx_enabled: bool,
        /// RX port override. Empty or absent names use the server default.
        #[serde(default)]
        rx_antenna: Option<String>,
        /// RX gain in dB. Leave unset to keep the driver gain or the remote server default.
        #[serde(default)]
        rx_gain_db: Option<f64>,
        /// RX-to-TX timestamp correction in nanoseconds. Add it once in the scheduler.
        /// See [`crate::RadioCalibration::rx_to_tx_delay_ns`] for the sign and clock requirements.
        #[serde(default)]
        rx_to_tx_delay_ns: Option<i64>,
    },
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self::Noop {}
    }
}

impl DeviceConfig {
    /// Whether the configuration allows RX. This does not check the device or open a stream.
    pub fn has_rx(&self) -> bool {
        match self {
            Self::Soapy { .. }
            | Self::Uhd { .. }
            | Self::Lime { .. }
            | Self::BladeRf { .. }
            | Self::Noop {} => true,
            Self::Network { rx_enabled, .. } => *rx_enabled,
            Self::FileOutput { .. } => false,
        }
    }

    /// The snake-case `kind` discriminator.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::FileOutput { .. } => "file_output",
            Self::Noop {} => "noop",
            Self::Soapy { .. } => "soapy",
            Self::Uhd { .. } => "uhd",
            Self::Lime { .. } => "lime",
            Self::BladeRf { .. } => "blade_rf",
            Self::Network { .. } => "network",
        }
    }

    /// Configured RX antenna or fallback: LNAW for Soapy/Lime, RX2 for UHD, and empty for other unset backends.
    pub fn rx_antenna_or_default(&self) -> String {
        match self {
            Self::Soapy { rx_antenna, .. } | Self::Lime { rx_antenna, .. } => {
                rx_antenna.clone().unwrap_or_else(|| "LNAW".to_string())
            }
            Self::Uhd { rx_antenna, .. } => rx_antenna.clone().unwrap_or_else(|| "RX2".to_string()),
            Self::BladeRf { rx_antenna, .. } | Self::Network { rx_antenna, .. } => {
                rx_antenna.clone().unwrap_or_default()
            }
            _ => String::new(),
        }
    }

    /// Configured TX gain in dB, converted to `f64` without querying hardware.
    pub fn tx_gain_db_f64(&self) -> Option<f64> {
        match self {
            Self::Soapy { tx_gain_db, .. } | Self::Uhd { tx_gain_db, .. } => Some(*tx_gain_db),
            Self::Lime { tx_gain_db, .. } => Some(*tx_gain_db as f64),
            Self::BladeRf { tx_gain_db, .. } => Some(*tx_gain_db as f64),
            _ => None,
        }
    }

    /// Configured RX gain in dB, converted to `f64` without querying hardware.
    pub fn rx_gain_db_f64(&self) -> Option<f64> {
        match self {
            Self::Soapy { rx_gain_db, .. }
            | Self::Uhd { rx_gain_db, .. }
            | Self::Network { rx_gain_db, .. } => *rx_gain_db,
            Self::Lime { rx_gain_db, .. } => rx_gain_db.map(|g| g as f64),
            Self::BladeRf { rx_gain_db, .. } => rx_gain_db.map(|g| g as f64),
            _ => None,
        }
    }

    /// Configured signed TX timing correction relative to RX in nanoseconds.
    pub fn rx_to_tx_delay_ns(&self) -> Option<i64> {
        match self {
            Self::Soapy {
                rx_to_tx_delay_ns, ..
            }
            | Self::Uhd {
                rx_to_tx_delay_ns, ..
            }
            | Self::Lime {
                rx_to_tx_delay_ns, ..
            }
            | Self::BladeRf {
                rx_to_tx_delay_ns, ..
            }
            | Self::Network {
                rx_to_tx_delay_ns, ..
            } => *rx_to_tx_delay_ns,
            _ => None,
        }
    }

    /// Estimated UHD connector power in dBm for unit-RMS TX samples. Adjusts the calibrated power for the configured gain.
    /// Returns `None` without both calibration values or when the result is non-finite.
    pub fn tx_full_scale_power_estimate_dbm(&self) -> Option<f32> {
        match self {
            Self::Uhd {
                tx_gain_db,
                tx_max_gain_db: Some(max_gain_db),
                tx_max_gain_power_estimate_dbm: Some(max_power_dbm),
                ..
            } => {
                let estimate = *max_power_dbm as f64 + tx_gain_db - max_gain_db;
                estimate.is_finite().then_some(estimate as f32)
            }
            _ => None,
        }
    }

    /// Input power in dBm corresponding to 0 dBFS. UHD gain calibration takes precedence over a direct reference.
    pub fn rx_reference_dbm(&self) -> Option<f64> {
        match self {
            Self::Uhd {
                rx_gain_db: Some(gain_db),
                rx_max_gain_db: Some(max_gain_db),
                rx_max_gain_reference_dbm: Some(max_reference_dbm),
                ..
            } => {
                let reference_dbm = max_reference_dbm + max_gain_db - gain_db;
                reference_dbm.is_finite().then_some(reference_dbm)
            }
            Self::Soapy {
                rx_reference_dbm, ..
            }
            | Self::Uhd {
                rx_reference_dbm, ..
            }
            | Self::Lime {
                rx_reference_dbm, ..
            }
            | Self::BladeRf {
                rx_reference_dbm, ..
            } => *rx_reference_dbm,
            _ => None,
        }
    }
}

const NOMINAL_RATE_TOLERANCE_PPM: f64 = 1.0;
const MIN_NOMINAL_RATE_TOLERANCE_HZ: f64 = 1.0;
const PARTS_PER_MILLION: f64 = 1_000_000.0;

/// How opening treats a driver's applied sample rate.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RatePolicy {
    /// Use the reported rate, even when it differs from the request.
    Accept,
    /// Require the exact requested rate.
    Exact,
    /// Require driver readback to round to the requested whole-Hz rate.
    NearestWholeHz,
    /// Allow up to 1 ppm of readback error, with a minimum allowance of 1 Hz.
    #[default]
    Nominal,
}
impl RatePolicy {
    pub(crate) fn check(self, actual: f64, requested: usize, name: &str) -> Result<(), Error> {
        crate::stream::positive_hz(actual, name)?;
        let matches = match self {
            Self::Accept => true,
            Self::Exact => actual == requested as f64,
            Self::NearestWholeHz => actual.round() == requested as f64,
            Self::Nominal => {
                let tolerance_hz = (requested as f64 * NOMINAL_RATE_TOLERANCE_PPM
                    / PARTS_PER_MILLION)
                    .max(MIN_NOMINAL_RATE_TOLERANCE_HZ);
                requested > 0 && (actual - requested as f64).abs() <= tolerance_hz
            }
        };
        if !matches {
            return Err(Error::invalid_setting(format!(
                "{name} requested={requested} Hz actual={actual} Hz"
            )));
        }
        Ok(())
    }
}

/// Flat settings, in JSON or YAML. Load, override fields, then call [`Self::open`].
/// Omit both RX stream fields for TX-only operation. Tune frequency after splitting.
#[derive(Clone, Debug, Default, Serialize)]
pub struct RadioConfig {
    /// Backend fields serialize at the top level alongside the stream fields.
    #[serde(flatten)]
    pub device: DeviceConfig,
    /// TX coercion policy. Defaults to allowing 1 ppm or 1 Hz of readback error, whichever is larger.
    pub tx_rate_policy: RatePolicy,
    /// RX coercion policy. Defaults to allowing 1 ppm or 1 Hz of readback error, whichever is larger.
    pub rx_rate_policy: RatePolicy,
    /// Requested TX sample rate in Hz. Must be positive before opening.
    pub tx_sample_rate_hz: usize,
    /// TX analog bandwidth in Hz. `None` keeps the driver setting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_bandwidth_hz: Option<usize>,
    /// Requested RX sample rate in Hz. Set together with `rx_bandwidth_hz`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_sample_rate_hz: Option<usize>,
    /// RX analog bandwidth in Hz. Set together with `rx_sample_rate_hz`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_bandwidth_hz: Option<usize>,
}

impl<'de> Deserialize<'de> for RadioConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct Streams {
            tx_sample_rate_hz: usize,
            tx_rate_policy: RatePolicy,
            rx_rate_policy: RatePolicy,
            tx_bandwidth_hz: Option<usize>,
            rx_sample_rate_hz: Option<usize>,
            rx_bandwidth_hz: Option<usize>,
        }
        let mut fields = serde_json::Map::<String, serde_json::Value>::deserialize(deserializer)?;
        let mut stream_fields = serde_json::Map::new();
        for name in [
            "tx_sample_rate_hz",
            "tx_rate_policy",
            "rx_rate_policy",
            "tx_bandwidth_hz",
            "rx_sample_rate_hz",
            "rx_bandwidth_hz",
        ] {
            if let Some(value) = fields.remove(name) {
                stream_fields.insert(name.to_owned(), value);
            }
        }
        let streams: Streams = serde_json::from_value(serde_json::Value::Object(stream_fields))
            .map_err(serde::de::Error::custom)?;
        let device = if fields.is_empty() {
            DeviceConfig::default()
        } else {
            serde_json::from_value(serde_json::Value::Object(fields))
                .map_err(serde::de::Error::custom)?
        };
        Ok(Self {
            device,
            tx_rate_policy: streams.tx_rate_policy,
            rx_rate_policy: streams.rx_rate_policy,
            tx_sample_rate_hz: streams.tx_sample_rate_hz,
            tx_bandwidth_hz: streams.tx_bandwidth_hz,
            rx_sample_rate_hz: streams.rx_sample_rate_hz,
            rx_bandwidth_hz: streams.rx_bandwidth_hz,
        })
    }
}

impl RadioConfig {
    /// Checks stream settings and calibration without touching files or hardware.
    /// Opening repeats validation after any runtime overrides.
    pub fn validate(&self) -> Result<(), Error> {
        Settings::from_config(self).map(|_| ())
    }

    /// Reads JSON or YAML, selected by the file extension, and applies a sibling
    /// `<stem>.local.<ext>` override if present. Returns file, parse or schema
    /// errors. This does not open hardware.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let value = crate::config_load::load_with_local_override(path)?;
        serde_json::from_value(value).map_err(|error| {
            let context = format!("{}: {error}", path.display());
            Error::from(error).context(context)
        })
    }

    /// Opens the configured device and prepares its streams without tuning frequency.
    /// You can override fields after loading and before calling this method.
    /// Returns errors for invalid settings, unavailable drivers or device setup failures.
    /// Configured calibration stays attached to the radio and its split handles.
    pub fn open(&self) -> Result<Box<dyn crate::Radio>, Error> {
        crate::open::open(self)
    }
}

impl DeviceConfig {
    /// Power and timing calibration resolved for the configured gains.
    /// Missing values allow the opened device to supply its own calibration.
    pub fn calibration(&self) -> crate::RadioCalibration {
        crate::RadioCalibration {
            rx_to_tx_delay_ns: self.rx_to_tx_delay_ns(),
            tx_full_scale_power_estimate_dbm: self.tx_full_scale_power_estimate_dbm(),
            rx_reference_dbm: self.rx_reference_dbm().map(|value| value as f32),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_radio_examples_parse_and_name_a_tx_rate() {
        let mut checked = 0;
        for entry in std::fs::read_dir("config").unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if !name.starts_with("radio_") || !name.ends_with(".json") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let config: RadioConfig =
                serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(config.tx_sample_rate_hz > 0, "{name} has no TX sample rate");
            if config.rx_sample_rate_hz.is_some() {
                assert!(config.device.has_rx(), "{name} sets RX on a TX-only device");
                assert!(
                    config.rx_bandwidth_hz.is_some(),
                    "{name} sets an RX rate without an RX bandwidth"
                );
            }
            config
                .validate()
                .unwrap_or_else(|e| panic!("{name} does not validate: {e}"));
            checked += 1;
        }
        assert_eq!(checked, 7, "every backend needs an example");
    }

    fn uhd_config(clock: Option<u64>, tx_rate: usize) -> RadioConfig {
        let mut value = serde_json::json!({
            "kind": "uhd", "device": "", "channel": 0, "antenna": "TX/RX",
            "tx_sample_rate_hz": tx_rate,
        });
        if let Some(clock) = clock {
            value["master_clock_rate"] = serde_json::json!(clock);
        }
        serde_json::from_value(value).expect("uhd config")
    }

    #[test]
    fn a_configured_clock_that_cannot_divide_a_rate_is_refused() {
        // 39.3216 MHz does not divide a 2 MHz host rate.
        let error = uhd_config(Some(39_321_600), 2_000_000)
            .validate()
            .unwrap_err()
            .to_string();
        assert!(error.contains("remainder 1321600"), "{error}");
        assert!(error.contains("master_clock_rate"), "{error}");

        // The same clock against a rate it divides exactly.
        assert!(uhd_config(Some(39_321_600), 1_966_080).validate().is_ok());
        // No clock configured hands the choice to the backend, which divides by
        // construction, so validation has nothing to reject.
        assert!(uhd_config(None, 2_000_000).validate().is_ok());
        assert!(uhd_config(Some(0), 2_000_000).validate().is_err());
    }

    #[test]
    fn a_configured_clock_must_also_divide_the_rx_rate() {
        let mut config = uhd_config(Some(39_321_600), 1_966_080);
        config.rx_sample_rate_hz = Some(2_000_000);
        config.rx_bandwidth_hz = Some(2_000_000);
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("RX sample rate"), "{error}");
    }

    #[test]
    fn a_deferred_rate_validates_except_for_a_file_sink() {
        let mut deferred = uhd_config(None, 2_000_000);
        deferred.tx_sample_rate_hz = 0;
        assert!(deferred.validate().is_ok());

        let file: RadioConfig = serde_json::from_value(serde_json::json!({
            "kind": "file_output", "path": "unused.wav",
        }))
        .expect("file config");
        let error = file.validate().unwrap_err().to_string();
        assert!(error.contains("needs a TX sample rate"), "{error}");
    }

    #[test]
    fn hardware_and_stream_fields_round_trip_at_the_top_level() {
        let json = serde_json::json!({
            "kind": "uhd", "device": "type=b200", "channel": 0,
            "antenna": "TX/RX", "master_clock_rate": 32_000_000,
            "tx_sample_rate_hz": 2_000_000, "tx_bandwidth_hz": 1_500_000,
            "rx_sample_rate_hz": 2_000_000, "rx_bandwidth_hz": 1_500_000,
            "rx_to_tx_delay_ns": -200
        });
        let config: RadioConfig = serde_json::from_value(json.clone()).unwrap();
        let serialized = serde_json::to_value(&config).unwrap();
        for (key, value) in json.as_object().unwrap() {
            assert_eq!(&serialized[key], value);
        }
        assert!(serialized.get("tx").is_none());
        assert!(serialized.get("rx").is_none());
        let restored: RadioConfig = serde_json::from_value(serialized).unwrap();
        assert_eq!(restored.tx_sample_rate_hz, config.tx_sample_rate_hz);
        assert_eq!(restored.device.rx_to_tx_delay_ns(), Some(-200));
    }

    #[test]
    fn yaml_loads_and_takes_a_yaml_local_override() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("sdr-hal-config-yaml-{unique}"));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("radio.yaml");
        std::fs::write(
            &path,
            "kind: noop\ntx_sample_rate_hz: 1000000\nrx_sample_rate_hz: 1000000\nrx_bandwidth_hz: 750000\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("radio.local.yaml"),
            "tx_sample_rate_hz: 2000000\nrx_sample_rate_hz: null\nrx_bandwidth_hz: null\n",
        )
        .unwrap();
        let loaded = RadioConfig::load(&path);
        let unsupported = RadioConfig::load(dir.join("radio.toml"));
        std::fs::remove_dir_all(dir).unwrap();

        let config = loaded.unwrap();
        assert_eq!(config.tx_sample_rate_hz, 2_000_000);
        assert!(config.rx_sample_rate_hz.is_none());
        assert!(matches!(config.device, DeviceConfig::Noop {}));
        assert!(
            unsupported
                .unwrap_err()
                .to_string()
                .contains("expected a .json, .yaml or .yml")
        );
    }

    #[test]
    fn yaml_rejects_unknown_fields_like_json() {
        let error = serde_yaml::from_str::<RadioConfig>(
            "kind: noop\ntx_sample_rate_hz: 2000000\nrx_batch_pcgs: 2\n",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("rx_batch_pcgs"), "{error}");
    }

    #[test]
    fn local_json_and_runtime_overrides_apply_before_opening() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("sdr-hal-config-{unique}"));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("radio.json");
        std::fs::write(
            &path,
            r#"{
            "kind":"noop", "tx_sample_rate_hz":1000000,
            "rx_sample_rate_hz":1000000,"rx_bandwidth_hz":750000
        }"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("radio.local.json"),
            r#"{"tx_sample_rate_hz":2000000,"rx_sample_rate_hz":null,"rx_bandwidth_hz":null}"#,
        )
        .unwrap();
        let loaded = RadioConfig::load(path);
        std::fs::remove_dir_all(dir).unwrap();
        let mut config = loaded.unwrap();
        assert_eq!(config.tx_sample_rate_hz, 2_000_000);
        assert!(config.rx_sample_rate_hz.is_none());
        config.tx_sample_rate_hz = 3_000_000;
        let opened = config.open().unwrap();
        assert_eq!(opened.tx_sample_rate_hz(), 3_000_000.0);
        assert_eq!(opened.rx_sample_rate_hz(), None);
    }
    #[test]
    fn nominal_rate_accepts_bladerf_rounding_but_rejects_coercion() {
        const REQUESTED: usize = 4_915_200;
        for actual in [4_915_199.0, 4_915_201.0, 4_915_195.1, 4_915_204.9] {
            assert!(RatePolicy::default().check(actual, REQUESTED, "RX").is_ok());
        }
        for actual in [
            4_915_195.0,
            4_915_205.0,
            4_900_000.0,
            0.0,
            -1.0,
            f64::NAN,
            f64::INFINITY,
        ] {
            assert!(
                RatePolicy::default()
                    .check(actual, REQUESTED, "RX")
                    .is_err()
            );
        }
        assert!(RatePolicy::default().check(99_999.0, 100_000, "TX").is_ok());
        assert!(
            RatePolicy::default()
                .check(99_998.0, 100_000, "TX")
                .is_err()
        );
        assert!(
            RatePolicy::Exact
                .check(4_915_199.0, REQUESTED, "RX")
                .is_err()
        );
        let config: RadioConfig =
            serde_json::from_str(r#"{"kind":"noop","tx_sample_rate_hz":4915200}"#).unwrap();
        assert!(matches!(config.rx_rate_policy, RatePolicy::Nominal));
        assert!(matches!(config.tx_rate_policy, RatePolicy::Nominal));
        assert!(matches!(
            serde_json::from_str::<RatePolicy>("\"nominal\"").unwrap(),
            RatePolicy::Nominal
        ));
    }

    #[test]
    fn rate_policies_reject_invalid_readback_and_control_coercion() {
        const REQUESTED: usize = 2_000_000;
        assert!(
            RatePolicy::Accept
                .check(1_999_999.25, REQUESTED, "TX")
                .is_ok()
        );
        assert!(
            RatePolicy::Exact
                .check(1_999_999.25, REQUESTED, "TX")
                .is_err()
        );
        assert!(
            RatePolicy::NearestWholeHz
                .check(2_000_000.25, REQUESTED, "RX")
                .is_ok()
        );
        assert!(
            RatePolicy::NearestWholeHz
                .check(1_999_999.25, REQUESTED, "RX")
                .is_err()
        );
        for value in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(RatePolicy::Accept.check(value, REQUESTED, "TX").is_err());
        }
    }

    #[test]
    fn uhd_software_tx_frequency_correction_defaults_on_and_can_be_disabled() {
        let mut value = serde_json::json!({
            "kind": "uhd",
            "device": "type=b200",
            "channel": 0,
            "antenna": "TX/RX"
        });
        let default: DeviceConfig = serde_json::from_value(value.clone()).unwrap();
        assert!(matches!(
            default,
            DeviceConfig::Uhd {
                software_tx_frequency_correction: true,
                ..
            }
        ));
        value["software_tx_frequency_correction"] = serde_json::json!(false);
        let disabled: DeviceConfig = serde_json::from_value(value).unwrap();
        assert!(matches!(
            disabled,
            DeviceConfig::Uhd {
                software_tx_frequency_correction: false,
                ..
            }
        ));
    }
}
