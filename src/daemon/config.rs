use std::net::SocketAddr;
use std::path::Path;

use crate::daemon::composite::CompositeConfig;
use crate::error::Error;
use serde::Deserialize;

/// Default daemon network configuration filename, relative to the process working directory.
pub const DEFAULT_CONFIG_PATH: &str = "config/radiod_network.json";
/// Default gRPC control TCP port.
pub const DEFAULT_CONTROL_PORT: u16 = 50710;
/// Default IQ port, shared by UDP and TCP listeners.
pub const DEFAULT_DATA_PORT: u16 = 50711;

fn default_listen() -> SocketAddr {
    SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
        DEFAULT_CONTROL_PORT,
    )
}

fn default_data_listen() -> SocketAddr {
    SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
        DEFAULT_DATA_PORT,
    )
}

/// Daemon listener addresses, and the composite stream when several clients share one
/// device. Unknown fields are rejected. Sample rates and the device clock are not
/// configured here, because a client negotiates them per session. Composite mode is the
/// one exception, since its device rate is shared and cannot follow any one client.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RadiodConfig {
    /// Control TCP bind address. Defaults to all IPv4 interfaces on port 50710.
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// IQ UDP and TCP bind address. Defaults to all IPv4 interfaces on port 50711.
    #[serde(default = "default_data_listen")]
    pub data_listen: SocketAddr,
    /// Shared device stream. Absent serves one client at a time with no frequency
    /// shifting or rate conversion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composite: Option<CompositeConfig>,
}

impl RadiodConfig {
    /// Loads JSON or YAML with a sibling local override. Returns read, parse, or schema errors.
    pub fn load_from_path(path: &Path) -> Result<Self, Error> {
        let merged = crate::config_load::load_with_local_override(path)?;
        let config: RadiodConfig = serde_json::from_value(merged)?;
        if let Some(composite) = &config.composite {
            composite.validate()?;
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMPOSITE: &str = r#"{
        "composite": {
            "device_rate_hz": 3888000,
            "tx_center_hz": 867520000,
            "rx_center_hz": 822520000,
            "tx_digital_backoff_db": -12.0
        }
    }"#;

    #[test]
    fn listeners_default_and_a_configured_rate_is_refused() {
        let config: RadiodConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(config.listen, default_listen());
        assert_eq!(config.data_listen, default_data_listen());
        assert!(config.composite.is_none());

        // The rate moved to the client, so a file that still names one is a stale config
        // rather than a harmless extra key.
        let stale = serde_json::from_str::<RadiodConfig>(r#"{"tx_sample_rate_hz": 2000000}"#);
        assert!(stale.is_err());
    }

    #[test]
    fn a_composite_block_loads_with_the_listener_defaults() {
        let config: RadiodConfig = serde_json::from_str(COMPOSITE).unwrap();
        let composite = config.composite.expect("composite block");
        assert_eq!(config.listen, default_listen());
        assert_eq!(composite.device_rate_hz, 3_888_000);
        assert_eq!(composite.tx_center_hz, 867_520_000);
        assert_eq!(composite.rx_center_hz, 822_520_000);
        composite.validate().unwrap();
    }

    #[test]
    fn the_shipped_daemon_examples_load_and_validate() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config");
        let mut seen = 0;
        for entry in std::fs::read_dir(&root).expect("config directory") {
            let path = entry.expect("directory entry").path();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if !name.starts_with("radiod_") || name.contains(".local.") {
                continue;
            }
            let config: RadiodConfig = serde_json::from_slice(&std::fs::read(&path).unwrap())
                .unwrap_or_else(|e| panic!("{name} does not load: {e}"));
            if let Some(composite) = &config.composite {
                composite
                    .validate()
                    .unwrap_or_else(|e| panic!("{name} does not validate: {e}"));
            }
            seen += 1;
        }
        assert!(seen >= 2, "only {seen} shipped daemon example(s) found");
    }

    #[test]
    fn the_shipped_composite_example_serves_both_real_clients() {
        use crate::daemon::composite::Direction;

        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("config")
            .join("radiod_composite.json");
        let config: RadiodConfig = serde_json::from_slice(&std::fs::read(&path).unwrap())
            .expect("the shipped example loads");
        let composite = config.composite.expect("a composite block");

        const WIDE_RATE_HZ: u64 = 648_000;
        const NARROW_RATE_HZ: u64 = 388_800;
        for (name, rate, tx_hz, rx_hz, ratio, offset_hz) in [
            (
                "wide",
                WIDE_RATE_HZ,
                860_850_000u64,
                815_850_000u64,
                15usize,
                -4_095_000i64,
            ),
            (
                "narrow",
                NARROW_RATE_HZ,
                869_040_000,
                824_040_000,
                25,
                4_095_000,
            ),
        ] {
            let tx = composite
                .plan_slice(Direction::Tx, rate as f64, tx_hz, Some(30_000))
                .unwrap_or_else(|e| panic!("{name} TX does not fit: {e}"));
            let rx = composite
                .plan_slice(Direction::Rx, rate as f64, rx_hz, Some(30_000))
                .unwrap_or_else(|e| panic!("{name} RX does not fit: {e}"));
            assert_eq!((tx.ratio, tx.offset_hz), (ratio, offset_hz), "{name} TX");
            assert_eq!((rx.ratio, rx.offset_hz), (ratio, offset_hz), "{name} RX");
        }

        assert_eq!(composite.grid_block_samples().unwrap(), 19_440);

        for rate in [WIDE_RATE_HZ, NARROW_RATE_HZ] {
            assert_eq!(rate * 15 % 1000, 0, "{rate} Hz is not whole per 15 ms");
        }

        assert_eq!(composite.rate_ratio(800_000.0).unwrap(), 243);
    }

    #[test]
    fn a_narrower_composite_serves_carriers_moved_to_their_band_edges() {
        use crate::daemon::composite::Direction;

        // Two carriers within 3.04 MHz fit a much narrower composite, which means less
        // interpolation and a smaller data plane.
        let composite = CompositeConfig {
            device_rate_hz: 3_888_000,
            tx_center_hz: 867_520_000,
            rx_center_hz: 822_520_000,
            tx_digital_backoff_db: -12.0,
            block_duration_ms: 5.0 / 6.0,
            tx_deadline_ms: 2.0,
        };
        composite.validate().unwrap();
        for (name, tx_hz, offset_hz) in [
            ("wide", 866_000_000u64, -1_520_000i64),
            ("narrow", 869_040_000, 1_520_000),
        ] {
            let plan = composite
                .plan_slice(Direction::Tx, 388_800.0, tx_hz, Some(30_000))
                .unwrap_or_else(|e| panic!("{name} does not fit: {e}"));
            assert_eq!((plan.ratio, plan.offset_hz), (10, offset_hz), "{name}");
        }
        assert_eq!(
            crate::daemon::composite::grid_block_samples(3_888_000).unwrap(),
            3_240
        );
    }

    #[test]
    fn a_composite_block_requires_every_field_and_rejects_extras() {
        for broken in [
            r#"{"composite": {}}"#,
            r#"{"composite": {"device_rate_hz": 3888000}}"#,
            r#"{"composite": {"device_rate_hz": 3888000, "tx_center_hz": 1, "rx_center_hz": 1, "tx_digital_backoff_db": 0.0, "guard_hz": 1000}}"#,
        ] {
            assert!(
                serde_json::from_str::<RadiodConfig>(broken).is_err(),
                "{broken}"
            );
        }
    }
}
