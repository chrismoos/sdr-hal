use std::path::PathBuf;

use clap::Parser;
use log::info;
use sdr_hal::config::{DeviceConfig, RadioConfig};
use sdr_hal::daemon::config::DEFAULT_CONFIG_PATH;
use sdr_hal::daemon::{RadiodConfig, RxDefaults, serve};

#[derive(Parser)]
#[command(about = "SDR radio server: serves a local SDR over the network")]
struct Args {
    #[arg(long, default_value = DEFAULT_CONFIG_PATH)]
    config: PathBuf,

    #[arg(
        long,
        visible_alias = "radio",
        help = "Hardware radio JSON configuration"
    )]
    radio_config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), sdr_hal::error::Error> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    let config = RadiodConfig::load_from_path(&args.config)?;
    let mut radio_config = RadioConfig::load(&args.radio_config)?;
    if matches!(radio_config.device, DeviceConfig::Network { .. }) {
        return Err("radiod requires a local radio backend, not another network radio".into());
    }
    info!("radiod: serving {} radio", radio_config.device.kind_name());

    // Rates and the device clock come from the client, so the device opens unconfigured
    // and the radio file's own stream settings are ignored. Composite mode is the
    // exception: the shared stream is the operator's, so the device opens configured and
    // the analog filter bandwidths stay whatever the radio file names.
    match &config.composite {
        Some(composite) => {
            let rate = usize::try_from(composite.device_rate_hz)
                .map_err(|_| "composite.device_rate_hz exceeds platform usize")?;
            radio_config.tx_sample_rate_hz = rate;
            if radio_config.device.has_rx() {
                radio_config.rx_sample_rate_hz = Some(rate);
            }
            // An absent analog bandwidth has to cover the whole composite span, not one
            // client's channel, or a carrier away from center is filtered out.
            if radio_config.tx_bandwidth_hz.is_none() {
                radio_config.tx_bandwidth_hz = Some(rate);
            }
            if radio_config.rx_bandwidth_hz.is_none() && radio_config.device.has_rx() {
                radio_config.rx_bandwidth_hz = Some(rate);
            }
        }
        None => {
            radio_config.tx_sample_rate_hz = 0;
            radio_config.tx_bandwidth_hz = None;
            radio_config.rx_sample_rate_hz = None;
            radio_config.rx_bandwidth_hz = None;
        }
    }
    let radio = radio_config.open()?;
    match &config.composite {
        Some(composite) => {
            // Refuse to come up on a device that cannot serve the plan, rather than
            // letting the first client discover it. A coerced rate moves every carrier
            // and a clock that does not divide the rate makes timestamps inexact.
            composite.verify_device(radio.as_ref())?;
            info!(
                "radiod: composite device open at {} Hz, TX center {} Hz, RX center {} Hz, {} clock ticks per sample",
                composite.device_rate_hz,
                composite.tx_center_hz,
                composite.rx_center_hz,
                radio.tick_rate() / composite.device_rate_hz
            );
        }
        None => info!("radiod: device open with no stream configured, waiting for a client"),
    }
    let rx_defaults = RxDefaults {
        antenna: radio_config.device.rx_antenna_or_default(),
        gain_db: radio_config.device.rx_gain_db_f64(),
    };
    serve(
        radio,
        radio_config.device.has_rx(),
        radio_config.device.kind_name().to_string(),
        rx_defaults,
        config.composite,
        config.listen,
        config.data_listen,
        None,
        async {
            let _ = tokio::signal::ctrl_c().await;
            info!("radiod: shutting down");
        },
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_default_listener_config_and_accepts_an_override() {
        let args = Args::try_parse_from([
            "sdr-radiod",
            "--radio-config",
            "config/radio_soapy_plutosdr.json",
        ])
        .unwrap();
        assert_eq!(args.config, PathBuf::from(DEFAULT_CONFIG_PATH));
        assert_eq!(
            args.radio_config,
            PathBuf::from("config/radio_soapy_plutosdr.json")
        );
        let args = Args::try_parse_from([
            "sdr-radiod",
            "--config",
            "custom.json",
            "--radio",
            "hardware.json",
        ])
        .unwrap();
        assert_eq!(args.config, PathBuf::from("custom.json"));
        assert_eq!(args.radio_config, PathBuf::from("hardware.json"));
        assert!(Args::try_parse_from(["sdr-radiod"]).is_err());
    }

    #[test]
    fn a_composite_flag_is_not_accepted_any_more() {
        // Verification happens at startup, so there is no separate mode to run.
        assert!(
            Args::try_parse_from(["sdr-radiod", "--radio-config", "hardware.json", "--check"])
                .is_err()
        );
    }
}
