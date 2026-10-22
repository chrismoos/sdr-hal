#[cfg(feature = "file")]
use crate::FileOutputRadio;
#[cfg(feature = "soapy-backend")]
use crate::SoapySdrRadio;
use crate::{
    NoopRadio, Radio,
    config::{DeviceConfig, RadioConfig},
    error::Error,
};
use log::info;
#[cfg(feature = "file")]
use std::fs;

pub(crate) fn open(config: &RadioConfig) -> Result<Box<dyn Radio>, Error> {
    let settings = crate::config::Settings::from_config(config)?;
    // A zero rate means the caller will negotiate one later.
    let tx_sample_rate_hz = (config.tx_sample_rate_hz > 0).then_some(config.tx_sample_rate_hz);
    let mut radio = open_radio(&config.device, tx_sample_rate_hz)?;
    if let Some(bandwidth_hz) = config.tx_bandwidth_hz {
        radio.set_tx_bandwidth(bandwidth_hz)?;
    }
    if let Some(rx) = settings.rx {
        let rate = rx.sample_rate_hz;
        let bandwidth = rx.bandwidth_hz;
        let antenna = config.device.rx_antenna_or_default();
        let channel = match &config.device {
            DeviceConfig::Soapy { channel, .. }
            | DeviceConfig::Uhd { channel, .. }
            | DeviceConfig::Lime { channel, .. } => *channel,
            DeviceConfig::BladeRf { channel, .. } => *channel as usize,
            _ => 0,
        };
        let actual = radio.setup_rx(
            channel,
            &antenna,
            rate as f64,
            bandwidth as f64,
            config.device.rx_gain_db_f64(),
        )?;
        config
            .rx_rate_policy
            .check(actual, rate, "RX sample rate")?;
        info!(
            "rx: prepared {} Hz stream, antenna={} bandwidth={} Hz",
            actual, antenna, bandwidth
        );
    }
    if let Some(requested) = tx_sample_rate_hz {
        config
            .tx_rate_policy
            .check(radio.tx_sample_rate_hz(), requested, "TX sample rate")?;
    }
    Ok(Box::new(
        crate::CalibratedRadio::new(radio, settings.calibration)?
            .with_tx_reference_gain_db(config.device.tx_gain_db_f64())?
            .with_rx_reference_gain_db(config.device.rx_gain_db_f64())?,
    ))
}

/// Opens the configured device. An absent rate leaves TX unconfigured, which lets a
/// caller that negotiates the rate later open the device first.
fn open_radio(
    config: &DeviceConfig,
    tx_sample_rate_hz: Option<usize>,
) -> Result<Box<dyn Radio>, Error> {
    match config {
        DeviceConfig::Noop {} => Ok(Box::new(NoopRadio::new(tx_sample_rate_hz.unwrap_or(0)))),
        #[cfg(feature = "file")]
        DeviceConfig::FileOutput { path } => {
            // Validation rejects a file sink with no rate, because the WAV header fixes
            // it. Creating the file first would truncate an existing capture.
            let rate = tx_sample_rate_hz
                .ok_or_else(|| Error::invalid_setting("file output needs a TX sample rate"))?;
            Ok(Box::new(FileOutputRadio::new(
                fs::File::create(path)?,
                rate,
            )?))
        }
        #[cfg(feature = "soapy-backend")]
        DeviceConfig::Soapy {
            device,
            channel,
            antenna,
            tx_gain_db,
            ..
        } => Ok(Box::new(SoapySdrRadio::new(
            device,
            *channel,
            antenna,
            *tx_gain_db,
            tx_sample_rate_hz,
        )?)), // rate stays optional
        #[cfg(feature = "uhd-backend")]
        DeviceConfig::Uhd {
            device,
            channel,
            antenna,
            tx_gain_db,
            master_clock_rate,
            max_master_clock_rate,
            clock_source,
            time_source,
            ..
        } => {
            let radio = crate::UhdRadio::new(
                device,
                *channel,
                antenna,
                *tx_gain_db,
                *master_clock_rate,
                *max_master_clock_rate,
                tx_sample_rate_hz,
            )?;
            if let Some(source) = clock_source {
                radio.set_clock_source(source)?;
            }
            if let Some(source) = time_source {
                radio.set_time_source(source)?;
            }
            Ok(Box::new(radio))
        }
        #[cfg(feature = "lime-backend")]
        DeviceConfig::Lime {
            device,
            channel,
            tx_antenna,
            tx_gain_db,
            oversample,
            tx_lo_offset_hz,
            tx_fifo_size,
            rx_fifo_size,
            stream_throughput_vs_latency,
            ..
        } => {
            let mut radio = crate::LimeRadio::with_stream_config(
                device,
                *channel,
                tx_antenna,
                *tx_gain_db,
                tx_sample_rate_hz,
                oversample.unwrap_or(0),
                *tx_fifo_size,
                *rx_fifo_size,
                *stream_throughput_vs_latency,
            )?;
            if let Some(offset) = tx_lo_offset_hz {
                radio.set_tx_lo_offset(*offset)?;
            }
            Ok(Box::new(radio))
        }
        #[cfg(feature = "bladerf-backend")]
        DeviceConfig::BladeRf {
            device,
            channel,
            fpga_path,
            tx_antenna,
            tx_gain_db,
            tx_lo_offset_hz,
            num_buffers,
            buffer_size,
            num_transfers,
            stream_timeout_ms,
            ..
        } => {
            let rate = tx_sample_rate_hz
                .map(|rate| {
                    u32::try_from(rate)
                        .map_err(|_| Error::from("bladeRF TX sample rate exceeds u32"))
                })
                .transpose()?;
            let mut radio = crate::BladeRfRadio::with_stream_config(
                device,
                *channel,
                *tx_gain_db,
                rate,
                fpga_path.as_deref(),
                tx_antenna.as_deref(),
                *num_buffers,
                *buffer_size,
                *num_transfers,
                *stream_timeout_ms,
            )?;
            if let Some(offset) = tx_lo_offset_hz {
                radio.set_tx_lo_offset(*offset)?;
            }
            Ok(Box::new(radio))
        }
        #[cfg(feature = "network-backend")]
        DeviceConfig::Network {
            addr,
            data_host,
            samples_per_packet,
            transport,
            tick_rate,
            ..
        } => {
            let mut radio = crate::NetworkRadio::connect(
                addr,
                crate::NetworkRadioOptions {
                    samples_per_packet: *samples_per_packet,
                    transport: *transport,
                    data_host: data_host.clone(),
                },
            )?;
            if let Some(tick_rate) = *tick_rate {
                let actual = radio.set_tick_rate(tick_rate)?;
                info!("network radio: device clock requested={tick_rate} actual={actual}");
            }
            if let Some(rate) = tx_sample_rate_hz {
                radio.set_tx_sample_rate(rate as f64)?;
            }
            Ok(Box::new(radio))
        }
        #[cfg(not(feature = "file"))]
        DeviceConfig::FileOutput { .. } => Err(Error::unsupported("file backend not compiled in")),
        #[cfg(not(feature = "soapy-backend"))]
        DeviceConfig::Soapy { .. } => Err(Error::unsupported("SoapySDR backend not compiled in")),
        #[cfg(not(feature = "uhd-backend"))]
        DeviceConfig::Uhd { .. } => Err(Error::unsupported("UHD backend not compiled in")),
        #[cfg(not(feature = "lime-backend"))]
        DeviceConfig::Lime { .. } => Err(Error::unsupported("LimeSuite backend not compiled in")),
        #[cfg(not(feature = "bladerf-backend"))]
        DeviceConfig::BladeRf { .. } => Err(Error::unsupported("bladeRF backend not compiled in")),
        #[cfg(not(feature = "network-backend"))]
        DeviceConfig::Network { .. } => Err(Error::unsupported("network backend not compiled in")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RATE_HZ: usize = 2_000_000;

    #[test]
    fn json_stream_settings_can_be_overridden_before_opening() {
        let mut config: RadioConfig = serde_json::from_str(
            r#"{
            "kind":"noop", "tx_sample_rate_hz":1000000,
            "rx_sample_rate_hz":2000000,"rx_bandwidth_hz":1500000
        }"#,
        )
        .unwrap();
        config.tx_sample_rate_hz = SAMPLE_RATE_HZ;
        let opened = config.open().unwrap();
        assert_eq!(opened.tx_sample_rate_hz(), SAMPLE_RATE_HZ as f64);
        assert_eq!(opened.rx_sample_rate_hz(), Some(SAMPLE_RATE_HZ as f64));
        assert!(opened.split().unwrap().1.is_some());
        config.rx_sample_rate_hz = None;
        config.rx_bandwidth_hz = None;
        let opened = config.open().unwrap();
        assert_eq!(opened.rx_sample_rate_hz(), None);
        assert!(opened.split().unwrap().1.is_none());
    }

    #[test]
    fn invalid_stream_settings_fail_before_creating_output_files() {
        let config = RadioConfig {
            device: DeviceConfig::FileOutput {
                path: String::new(),
            },
            tx_sample_rate_hz: SAMPLE_RATE_HZ,
            tx_bandwidth_hz: None,
            rx_sample_rate_hz: Some(SAMPLE_RATE_HZ),
            rx_bandwidth_hz: Some(SAMPLE_RATE_HZ),
            ..Default::default()
        };
        assert!(
            config
                .open()
                .err()
                .unwrap()
                .to_string()
                .contains("does not allow RX")
        );
        // No TX rate is now a deferral, which a backend that negotiates later allows.
        assert!(RadioConfig::default().open().is_ok());
        // A WAV header fixes the rate, so a file sink still has to be told one.
        let deferred_file = RadioConfig {
            device: DeviceConfig::FileOutput {
                path: String::new(),
            },
            ..Default::default()
        };
        assert!(
            deferred_file
                .open()
                .err()
                .unwrap()
                .to_string()
                .contains("needs a TX sample rate")
        );
        assert!(
            crate::config::RatePolicy::NearestWholeHz
                .check(SAMPLE_RATE_HZ as f64 + 0.25, SAMPLE_RATE_HZ, "RX")
                .is_ok()
        );
        assert!(
            crate::config::RatePolicy::NearestWholeHz
                .check(SAMPLE_RATE_HZ as f64 + 1.0, SAMPLE_RATE_HZ, "RX")
                .is_err()
        );
    }

    #[test]
    fn incomplete_rx_settings_are_rejected_before_opening() {
        let mut config = RadioConfig {
            tx_sample_rate_hz: SAMPLE_RATE_HZ,
            rx_sample_rate_hz: Some(SAMPLE_RATE_HZ),
            ..Default::default()
        };
        assert!(config.open().is_err());
        config.rx_sample_rate_hz = None;
        config.rx_bandwidth_hz = Some(SAMPLE_RATE_HZ);
        assert!(config.open().is_err());
    }

    #[test]
    fn configuration_rejects_unknown_fields_in_each_section() {
        for json in [
            r#"{"unexpected":1}"#,
            r#"{"kind":"noop","unexpected":1}"#,
            r#"{"kind":"noop","tx_sample_rate_hz":1,"unexpected":1}"#,
            r#"{"kind":"noop","rx_sample_rate_hz":1,"rx_bandwidth_hz":1,"unexpected":1}"#,
        ] {
            assert!(serde_json::from_str::<RadioConfig>(json).is_err(), "{json}");
        }
    }

    #[test]
    fn calibrated_gain_references_and_signed_correction_survive_splitting() {
        let device: DeviceConfig = serde_json::from_value(serde_json::json!({
            "kind": "uhd", "device": "", "channel": 0, "antenna": "TX/RX",
            "master_clock_rate": 40_000_000, "tx_gain_db": 50.0,
            "tx_max_gain_db": 60.0, "tx_max_gain_power_estimate_dbm": 10.0,
            "rx_gain_db": 30.0, "rx_max_gain_db": 40.0,
            "rx_max_gain_reference_dbm": -40.0, "rx_reference_dbm": -99.0,
            "rx_to_tx_delay_ns": -70
        }))
        .unwrap();
        let mut radio = NoopRadio::new(SAMPLE_RATE_HZ);
        radio
            .setup_rx(0, "", SAMPLE_RATE_HZ as f64, SAMPLE_RATE_HZ as f64, None)
            .unwrap();
        let radio =
            Box::new(crate::CalibratedRadio::new(Box::new(radio), device.calibration()).unwrap());
        assert_eq!(radio.rx_to_tx_delay_ns(), Some(-70));
        let (tx, rx) = radio.split().unwrap();
        assert_eq!(tx.tx_full_scale_power_estimate_dbm(), Some(0.0));
        assert_eq!(tx.rx_to_tx_delay_ns(), Some(-70));
        assert_eq!(rx.unwrap().rx_reference_dbm(), Some(-30.0));
    }
    #[cfg(feature = "file")]
    #[test]
    fn invalid_wav_rate_preserves_an_existing_output_file() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "sdr-hal-invalid-rate-{}-{suffix}.wav",
            std::process::id()
        ));
        std::fs::write(&path, b"existing capture").unwrap();
        let config = RadioConfig {
            device: DeviceConfig::FileOutput {
                path: path.to_string_lossy().into_owned(),
            },
            tx_sample_rate_hz: (u32::MAX as usize).checked_add(1).unwrap_or(0),
            ..Default::default()
        };
        assert_eq!(
            config.open().err().unwrap().kind(),
            crate::error::ErrorKind::InvalidSetting
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"existing capture");
        std::fs::remove_file(path).unwrap();
    }
}
