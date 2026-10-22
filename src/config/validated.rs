use super::{DeviceConfig, RadioConfig};
use crate::{RadioCalibration, error::Error};

pub(crate) struct RxSettings {
    pub sample_rate_hz: usize,
    pub bandwidth_hz: usize,
}
pub(crate) struct Settings {
    pub rx: Option<RxSettings>,
    pub calibration: RadioCalibration,
}

/// Rejects a configured clock that is not a whole multiple of a configured rate.
fn check_pinned_clock(clock_hz: u64, sample_rate_hz: usize, name: &str) -> Result<(), Error> {
    if sample_rate_hz == 0 {
        return Ok(());
    }
    if let Err(remainder) = crate::clock::tick_alignment(clock_hz, sample_rate_hz as u64) {
        return Err(Error::invalid_setting(format!(
            "UHD master clock rate {clock_hz} Hz is not a whole multiple of {name} sample rate {sample_rate_hz} Hz, remainder {remainder} Hz. Omit master_clock_rate to select one automatically."
        )));
    }
    Ok(())
}

impl Settings {
    pub fn from_config(config: &RadioConfig) -> Result<Self, Error> {
        // A zero TX rate defers the rate to a later call, which is what lets a daemon
        // open a device before a client has chosen one. Backends that bake the rate into
        // a header or a stream reject it below.
        if let Some(bandwidth) = config.tx_bandwidth_hz {
            crate::stream::positive_hz(bandwidth as f64, "TX bandwidth")?;
        }
        match &config.device {
            DeviceConfig::FileOutput { .. } if config.tx_sample_rate_hz == 0 => {
                return Err(Error::invalid_setting(
                    "file output needs a TX sample rate, because a WAV header cannot change later",
                ));
            }
            DeviceConfig::FileOutput { .. } if config.tx_sample_rate_hz > u32::MAX as usize => {
                return Err(Error::invalid_setting("WAV sample rate exceeds u32"));
            }
            DeviceConfig::BladeRf { .. } => {
                for (value, name) in [
                    (Some(config.tx_sample_rate_hz), "TX rate"),
                    (config.tx_bandwidth_hz, "TX bandwidth"),
                    (config.rx_sample_rate_hz, "RX rate"),
                    (config.rx_bandwidth_hz, "RX bandwidth"),
                ] {
                    if value.is_some_and(|value| value > u32::MAX as usize) {
                        return Err(Error::invalid_setting(format!(
                            "bladeRF {name} exceeds u32"
                        )));
                    }
                }
            }
            DeviceConfig::Lime { channel, .. } if *channel > u32::MAX as usize => {
                return Err(Error::invalid_setting("Lime channel exceeds u32"));
            }
            DeviceConfig::Uhd {
                master_clock_rate: Some(0),
                ..
            } => {
                return Err(Error::invalid_setting(
                    "UHD master clock rate must be positive",
                ));
            }
            DeviceConfig::Uhd {
                master_clock_rate: Some(clock),
                ..
            } => {
                // A configured clock is applied as given, so a rate it cannot divide
                // loses sub-tick accuracy on every timestamp. Omitting the clock picks
                // one that divides instead.
                check_pinned_clock(*clock, config.tx_sample_rate_hz, "TX")?;
                if let Some(rx_rate) = config.rx_sample_rate_hz {
                    check_pinned_clock(*clock, rx_rate, "RX")?;
                }
            }
            _ => {}
        }
        let rx = match (config.rx_sample_rate_hz, config.rx_bandwidth_hz) {
            (None, None) => None,
            (Some(rate), Some(bandwidth)) => {
                crate::stream::positive_hz(rate as f64, "RX sample rate")?;
                crate::stream::positive_hz(bandwidth as f64, "RX bandwidth")?;
                if !config.device.has_rx() {
                    return Err(Error::invalid_setting(
                        "the configured device does not allow RX",
                    ));
                }
                Some(RxSettings {
                    sample_rate_hz: rate,
                    bandwidth_hz: bandwidth,
                })
            }
            _ => {
                return Err(Error::invalid_setting(
                    "set both rx_sample_rate_hz and rx_bandwidth_hz, or omit both",
                ));
            }
        };
        if let Some(gain) = config.device.rx_gain_db_f64() {
            crate::stream::finite(gain, "RX gain")?;
        }
        if let DeviceConfig::Uhd {
            tx_gain_db,
            tx_max_gain_db,
            tx_max_gain_power_estimate_dbm,
            rx_max_gain_db,
            rx_max_gain_reference_dbm,
            ..
        } = &config.device
        {
            crate::stream::finite(*tx_gain_db, "TX gain")?;
            if tx_max_gain_db.is_some() != tx_max_gain_power_estimate_dbm.is_some() {
                return Err(Error::invalid_setting(
                    "set both TX calibration gain and power",
                ));
            }
            if rx_max_gain_db.is_some() != rx_max_gain_reference_dbm.is_some() {
                return Err(Error::invalid_setting(
                    "set both RX calibration gain and reference",
                ));
            }
            for (value, name) in [
                (*tx_max_gain_db, "TX calibration gain"),
                (*rx_max_gain_db, "RX calibration gain"),
                (*rx_max_gain_reference_dbm, "RX calibration reference"),
            ] {
                if let Some(value) = value {
                    crate::stream::finite(value, name)?;
                }
            }
            if let Some(value) = tx_max_gain_power_estimate_dbm {
                crate::stream::finite(*value as f64, "TX calibration power")?;
            }
        }
        match &config.device {
            DeviceConfig::Soapy { tx_gain_db, .. } => {
                crate::stream::finite(*tx_gain_db, "TX gain")?;
            }
            DeviceConfig::Lime {
                tx_fifo_size,
                rx_fifo_size,
                stream_throughput_vs_latency,
                ..
            } => {
                if *tx_fifo_size == Some(0) || *rx_fifo_size == Some(0) {
                    return Err(Error::invalid_setting("FIFO capacities must be positive"));
                }
                if let Some(preference) = stream_throughput_vs_latency
                    && (!preference.is_finite() || !(0.0..=1.0).contains(preference))
                {
                    return Err(Error::invalid_setting(
                        "stream throughput preference must be between zero and one",
                    ));
                }
            }
            DeviceConfig::BladeRf {
                num_buffers,
                buffer_size,
                num_transfers,
                ..
            } => {
                const BUFFER_ALIGNMENT_SAMPLES: u32 = 1024;
                if num_buffers.is_some_and(|n| n == 0)
                    || num_transfers.is_some_and(|n| n == 0)
                    || buffer_size
                        .is_some_and(|n| n == 0 || !n.is_multiple_of(BUFFER_ALIGNMENT_SAMPLES))
                {
                    return Err(Error::invalid_setting(
                        "invalid bladeRF buffer configuration",
                    ));
                }
                if let (Some(buffers), Some(transfers)) = (num_buffers, num_transfers)
                    && transfers >= buffers
                {
                    return Err(Error::invalid_setting(
                        "bladeRF transfers must be less than buffer count",
                    ));
                }
            }
            DeviceConfig::Network {
                samples_per_packet, ..
            } if (*samples_per_packet == 0 || *samples_per_packet > u32::MAX as usize) => {
                return Err(Error::invalid_setting(
                    "packet sample count must be positive and fit u32",
                ));
            }
            _ => {}
        }
        let calibration = config.device.calibration();
        for (value, name) in [
            (
                calibration.tx_full_scale_power_estimate_dbm,
                "TX power reference",
            ),
            (calibration.rx_reference_dbm, "RX power reference"),
        ] {
            if let Some(value) = value {
                crate::stream::finite(value as f64, name)?;
            }
        }
        if let DeviceConfig::Uhd {
            tx_max_gain_db: Some(_),
            tx_max_gain_power_estimate_dbm: Some(_),
            ..
        } = config.device
            && calibration.tx_full_scale_power_estimate_dbm.is_none()
        {
            return Err(Error::invalid_setting(
                "derived TX calibration is not finite",
            ));
        }
        if let DeviceConfig::Uhd {
            rx_gain_db: Some(_),
            rx_max_gain_db: Some(_),
            rx_max_gain_reference_dbm: Some(_),
            ..
        } = config.device
            && calibration.rx_reference_dbm.is_none()
        {
            return Err(Error::invalid_setting(
                "derived RX calibration is not finite",
            ));
        }
        Ok(Self { rx, calibration })
    }
}
