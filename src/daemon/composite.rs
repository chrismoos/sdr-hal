//! Composite mode: one device stream shared by several client sessions.
//!
//! The operator fixes the device rate and the TX and RX center frequencies. Each client
//! then configures its own rate and carrier as if it owned the radio, and the daemon
//! places that client's samples into the shared stream. A client never learns the device
//! rate, so nothing about this mode reaches the wire protocol.

use serde::Deserialize;

mod clients;
mod grid;
mod resample;
mod rx;
mod slice;
mod stages;
pub(crate) use clients::ClientRegistry;
pub(crate) use grid::{Staged, TxGrid};
pub(crate) use rx::RxFanout;
use slice::{RxSlice, TxSlice};
pub use stages::IntermediateStage;

use crate::dsp::design;
use crate::error::Error;

/// Fraction of a client's own Nyquist that stays unfiltered when it states no bandwidth.
/// The remainder becomes the anti-alias transition, which cannot be zero.
const DEFAULT_PASSBAND_FRACTION: f64 = 0.8;

/// Bounds integer interpolation and rational phase-bank storage.
const MAX_RATE_RATIO: u64 = 4096;

/// A fractional client rate matches a device fraction when it lands this close in Hz.
/// `f64` holds a rate such as 3,250,000/3 Hz to about 1e-10 Hz.
const RATE_MATCH_TOLERANCE_HZ: f64 = 1e-6;

/// Continued-fraction terms tried before giving up on a fractional rate.
const MAX_FRACTION_TERMS: usize = 64;

const DEFAULT_BLOCK_DURATION_MS: f64 = 2.0;
const DEFAULT_TX_DEADLINE_MS: f64 = 5.0;
const MAX_TIMING_MS: f64 = 1000.0;

fn default_block_duration_ms() -> f64 {
    DEFAULT_BLOCK_DURATION_MS
}
fn default_tx_deadline_ms() -> f64 {
    DEFAULT_TX_DEADLINE_MS
}

/// Widest grid block, which bounds both the summing buffer and the scheduling lead.
pub(crate) const MAX_GRID_BLOCK_SAMPLES: usize = 1 << 20;

/// Operator-configured composite stream.
///
/// `device_rate_hz` is the width of the shared stream, not a rate any client has to use.
/// The analog filter bandwidths come from the radio configuration, as they do for any
/// other device-level setting.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompositeConfig {
    /// Shared device sample rate in Hz, which sets the span carriers must fit inside.
    pub device_rate_hz: u64,
    /// TX local-oscillator frequency in Hz. Client carriers are offsets from this.
    pub tx_center_hz: u64,
    /// RX local-oscillator frequency in Hz. Client carriers are offsets from this.
    pub rx_center_hz: u64,
    /// Digital headroom applied to every slice, in dB at or below zero. It does not
    /// depend on how many clients are attached, so a client's output level never moves
    /// when a peer connects or leaves.
    pub tx_digital_backoff_db: f64,
    /// Output block duration in milliseconds, rounded to whole device samples.
    #[serde(default = "default_block_duration_ms")]
    pub block_duration_ms: f64,
    /// Milliseconds before output starts at which missing client samples become silence.
    #[serde(default = "default_tx_deadline_ms")]
    pub tx_deadline_ms: f64,
}

/// Which direction a slice runs, for error text and for picking the center.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Client samples summed into the device stream.
    Tx,
    /// Device samples sliced out for one client.
    Rx,
}

impl Direction {
    fn label(self) -> &'static str {
        match self {
            Self::Tx => "TX",
            Self::Rx => "RX",
        }
    }
}

/// One client's place in the composite stream.
#[derive(Clone, Debug, PartialEq)]
pub struct SlicePlan {
    /// Numerator of device samples per client sample, reduced to lowest terms.
    pub ratio: usize,
    /// Denominator of device samples per client sample on the exact rate lattice.
    pub denominator: usize,
    /// One-sided occupied bandwidth used to design rational conversion filters.
    pub passband_hz: f64,
    /// Signed shift from the shared center, in Hz.
    pub offset_hz: i64,
    /// Device-rate taps for integer conversion. Empty for a rational phase bank.
    pub taps: Vec<f64>,
    /// Integer step to the device rate. `None` converts in one step.
    pub intermediate: Option<IntermediateStage>,
}

impl CompositeConfig {
    /// Rejects a plan that cannot describe a stream.
    pub fn validate(&self) -> Result<(), Error> {
        if self.device_rate_hz == 0 {
            return Err("composite.device_rate_hz must be positive".into());
        }
        if self.tx_center_hz == 0 {
            return Err("composite.tx_center_hz must be positive".into());
        }
        if self.rx_center_hz == 0 {
            return Err("composite.rx_center_hz must be positive".into());
        }
        if !self.tx_digital_backoff_db.is_finite() || self.tx_digital_backoff_db > 0.0 {
            return Err(format!(
                "composite.tx_digital_backoff_db must be a finite value at or below zero, not {}",
                self.tx_digital_backoff_db
            )
            .into());
        }
        for (name, value, minimum) in [
            (
                "block_duration_ms",
                self.block_duration_ms,
                f64::MIN_POSITIVE,
            ),
            ("tx_deadline_ms", self.tx_deadline_ms, 0.0),
        ] {
            if !value.is_finite() || value < minimum || value > MAX_TIMING_MS {
                return Err(format!("composite.{name} must be finite and between {minimum} and {MAX_TIMING_MS} ms, not {value}").into());
            }
        }
        self.grid_block_samples().map(|_| ())
    }

    pub(crate) fn grid_block_samples(&self) -> Result<usize, Error> {
        grid_block_samples_at_duration(self.device_rate_hz, self.block_duration_ms)
    }

    /// Checks a device against this plan, naming every reason it cannot serve it.
    ///
    /// Reads back what the driver applied rather than trusting the request, because a
    /// coerced rate moves every carrier offset and a clock that does not divide the rate
    /// makes timestamps inexact. The caller has already configured the device, so this
    /// touches nothing and never transmits.
    pub fn verify_device(&self, radio: &dyn crate::Radio) -> Result<(), Error> {
        let applied_tx = radio.tx_sample_rate_hz();
        let applied_rx = radio.rx_sample_rate_hz();
        let tick_rate = radio.tick_rate();
        let mut problems = Vec::new();

        if applied_tx.round() as u64 != self.device_rate_hz {
            problems.push(format!(
                "the device applied {applied_tx} Hz for TX rather than {} Hz, which would move every carrier offset",
                self.device_rate_hz
            ));
        }
        if let Some(rx) = applied_rx
            && rx.round() as u64 != self.device_rate_hz
        {
            problems.push(format!(
                "the device applied {rx} Hz for RX rather than {} Hz",
                self.device_rate_hz
            ));
        }
        if tick_rate == 0 || !tick_rate.is_multiple_of(self.device_rate_hz) {
            problems.push(format!(
                "the device clock {tick_rate} Hz is not a whole multiple of {} Hz, so composite timestamps cannot be exact. Omit master_clock_rate so the backend picks a clock that divides the rate, or pin one that does",
                self.device_rate_hz
            ));
        }

        if problems.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "this device cannot serve the composite plan: {}",
                problems.join("; ")
            )
            .into())
        }
    }

    /// Linear amplitude every slice is scaled by before summing.
    pub fn tx_scale(&self) -> f32 {
        10f64.powf(self.tx_digital_backoff_db / 20.0) as f32
    }

    fn center_hz(&self, direction: Direction) -> u64 {
        match direction {
            Direction::Tx => self.tx_center_hz,
            Direction::Rx => self.rx_center_hz,
        }
    }

    /// Client sample rates this plan can accept in Hz, inclusive.
    pub fn client_rate_range_hz(&self) -> (u64, u64) {
        (
            self.device_rate_hz.div_ceil(MAX_RATE_RATIO),
            self.device_rate_hz,
        )
    }

    /// Carrier frequencies this plan can accept in Hz, inclusive.
    pub fn frequency_range_hz(&self, direction: Direction) -> (u64, u64) {
        let center = self.center_hz(direction);
        let half_span = self.device_rate_hz / 2;
        (
            center.saturating_sub(half_span),
            center.saturating_add(half_span),
        )
    }

    /// Reduced numerator of the device/client rate ratio, bounded to limit filter storage.
    pub fn rate_ratio(&self, client_rate_hz: f64) -> Result<usize, Error> {
        self.rate_terms(client_rate_hz).map(|(ratio, _)| ratio)
    }

    /// The rate this plan runs a client asking for `client_rate_hz` at, which is exact
    /// for a whole rate and the matched device fraction for a fractional one.
    pub fn exact_client_rate_hz(&self, client_rate_hz: f64) -> Result<f64, Error> {
        let (ratio, denominator) = self.rate_terms(client_rate_hz)?;
        Ok(self.client_rate_hz(ratio, denominator))
    }

    /// The client rate a reduced `ratio / denominator` describes, in Hz.
    pub fn client_rate_hz(&self, ratio: usize, denominator: usize) -> f64 {
        self.device_rate_hz as f64 * denominator as f64 / ratio as f64
    }

    /// Device/client rate as a reduced `(ratio, denominator)`, both at most
    /// [`MAX_RATE_RATIO`]. A fractional rate is matched to the nearest such fraction.
    fn rate_terms(&self, client_rate_hz: f64) -> Result<(usize, usize), Error> {
        let device = self.device_rate_hz;
        if !client_rate_hz.is_finite() || client_rate_hz <= 0.0 || client_rate_hz > device as f64 {
            return Err(format!(
                "client rate {client_rate_hz} Hz is not positive or exceeds device rate {device} Hz"
            )
            .into());
        }
        let (numerator, denominator) = if client_rate_hz.fract() == 0.0 {
            let client = client_rate_hz as u64;
            let divisor = gcd(device, client);
            (device / divisor, client / divisor)
        } else {
            device_fraction(device, client_rate_hz).ok_or_else(|| -> Error {
                format!(
                    "client rate {client_rate_hz} Hz is not a fraction of device rate {device} Hz with terms up to {MAX_RATE_RATIO}"
                )
                .into()
            })?
        };
        if numerator > MAX_RATE_RATIO || denominator > MAX_RATE_RATIO {
            return Err(format!("client rate {client_rate_hz} Hz needs ratio {numerator}/{denominator}, past the {MAX_RATE_RATIO} limit").into());
        }
        Ok((numerator as usize, denominator as usize))
    }

    /// Places one client in the composite stream.
    ///
    /// `bandwidth_hz` is the client's own occupied bandwidth. It sizes the slice filter,
    /// so a client that states a narrow bandwidth gets a shorter filter and a wider
    /// transition than one that asks for its whole band.
    pub fn plan_slice(
        &self,
        direction: Direction,
        client_rate_hz: f64,
        carrier_hz: u64,
        bandwidth_hz: Option<u64>,
    ) -> Result<SlicePlan, Error> {
        let (ratio, denominator) = self.rate_terms(client_rate_hz)?;
        let client_rate_hz = self.client_rate_hz(ratio, denominator);
        let offset_hz = self.offset_hz(direction, client_rate_hz, carrier_hz)?;
        let passband_hz = bandwidth_hz
            .map(|hz| hz as f64 / 2.0)
            .unwrap_or(client_rate_hz * DEFAULT_PASSBAND_FRACTION / 2.0)
            .min(client_rate_hz * DEFAULT_PASSBAND_FRACTION / 2.0)
            .max(1.0);
        let (taps, intermediate) = if denominator == 1 {
            (self.slice_taps(ratio, client_rate_hz, bandwidth_hz), None)
        } else {
            let stage = stages::choose(
                direction,
                self.device_rate_hz,
                ratio,
                denominator,
                passband_hz,
            )?;
            (Vec::new(), stage)
        };
        Ok(SlicePlan {
            ratio,
            denominator,
            passband_hz,
            offset_hz,
            taps,
            intermediate,
        })
    }

    /// Signed shift from the shared center, rejecting a carrier whose own band would
    /// reach past the composite edge.
    ///
    /// The check uses the client's sample rate as its widest possible occupied bandwidth,
    /// so it needs no guard constant and tightens for a client that samples slowly.
    pub fn offset_hz(
        &self,
        direction: Direction,
        client_rate_hz: f64,
        carrier_hz: u64,
    ) -> Result<i64, Error> {
        let center = self.center_hz(direction);
        if carrier_hz == 0 {
            return Err(
                format!("a composite {} carrier must be positive", direction.label()).into(),
            );
        }
        let offset_hz = i64::try_from(carrier_hz)
            .ok()
            .zip(i64::try_from(center).ok())
            .and_then(|(carrier, center)| carrier.checked_sub(center))
            .ok_or_else(|| -> Error {
                format!(
                    "composite {} carrier {carrier_hz} Hz is out of range",
                    direction.label()
                )
                .into()
            })?;

        let reach_hz = offset_hz.unsigned_abs() as f64 + client_rate_hz / 2.0;
        let half_span_hz = self.device_rate_hz as f64 / 2.0;
        if reach_hz > half_span_hz {
            return Err(format!(
                "composite {} carrier {carrier_hz} Hz sits {offset_hz} Hz from center {center} Hz, and at {client_rate_hz} Hz it reaches {reach_hz} Hz past center, beyond the {half_span_hz} Hz half-span of a {} Hz composite",
                direction.label(),
                self.device_rate_hz
            )
            .into());
        }
        Ok(offset_hz)
    }

    fn slice_taps(&self, ratio: usize, client_rate_hz: f64, bandwidth_hz: Option<u64>) -> Vec<f64> {
        if ratio < 2 {
            // No rate change, so there is nothing to alias and no filter to apply.
            return vec![1.0];
        }
        let client_nyquist_hz = client_rate_hz / 2.0;
        let default_hz = DEFAULT_PASSBAND_FRACTION * client_nyquist_hz;
        let requested_hz = bandwidth_hz
            .map(|hz| hz as f64 / 2.0)
            .unwrap_or(default_hz)
            .min(default_hz)
            .max(1.0);
        design::rate_change_lowpass(ratio, requested_hz / self.device_rate_hz as f64)
    }
}

#[cfg(test)]
pub(crate) fn grid_block_samples(device_rate_hz: u64) -> Result<usize, Error> {
    grid_block_samples_at_duration(device_rate_hz, 5.0 / 6.0)
}

fn grid_block_samples_at_duration(device_rate_hz: u64, duration_ms: f64) -> Result<usize, Error> {
    let block = (device_rate_hz as f64 * duration_ms / 1000.0).round();
    if block < 1.0 {
        return Err(format!(
            "composite.block_duration_ms of {duration_ms} ms is under one sample at {device_rate_hz} Hz"
        )
        .into());
    }
    if block > MAX_GRID_BLOCK_SAMPLES as f64 {
        return Err(format!(
            "composite.block_duration_ms of {duration_ms} ms needs a {block}-sample grid block, past the {MAX_GRID_BLOCK_SAMPLES} limit"
        )
        .into());
    }
    Ok(block as usize)
}

/// Smallest-term fraction `(ratio, denominator)` with `device * denominator / ratio`
/// within [`RATE_MATCH_TOLERANCE_HZ`] of `client_rate_hz`, from the continued fraction of
/// `device / client_rate_hz`.
fn device_fraction(device: u64, client_rate_hz: f64) -> Option<(u64, u64)> {
    let target = device as f64 / client_rate_hz;
    let (mut previous, mut current) = ((0u64, 1u64), (1u64, 0u64));
    let mut remainder = target;
    for _ in 0..MAX_FRACTION_TERMS {
        let term = remainder.floor();
        if term > MAX_RATE_RATIO as f64 {
            return None;
        }
        let term = term as u64;
        let next = (term * current.0 + previous.0, term * current.1 + previous.1);
        if next.0 > MAX_RATE_RATIO || next.1 > MAX_RATE_RATIO {
            return None;
        }
        let rate = device as f64 * next.1 as f64 / next.0 as f64;
        if (rate - client_rate_hz).abs() <= RATE_MATCH_TOLERANCE_HZ {
            return Some(next);
        }
        let fraction = remainder - term as f64;
        if fraction <= f64::EPSILON {
            return None;
        }
        (previous, current) = (current, next);
        remainder = 1.0 / fraction;
    }
    None
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two carriers 3.04 MHz apart about a shared center.
    const PLAN: CompositeConfig = CompositeConfig {
        device_rate_hz: 3_888_000,
        tx_center_hz: 867_520_000,
        rx_center_hz: 822_520_000,
        tx_digital_backoff_db: -12.0,
        block_duration_ms: 5.0 / 6.0,
        tx_deadline_ms: 2.0,
    };
    const CLIENT_RATE_HZ: u64 = 388_800;

    #[test]
    fn timing_defaults_and_overrides_set_the_block_length_exactly() {
        let config: CompositeConfig = serde_json::from_str(r#"{"device_rate_hz":9720000,"tx_center_hz":865200000,"rx_center_hz":820200000,"tx_digital_backoff_db":-3}"#).unwrap();
        config.validate().unwrap();
        assert_eq!(config.block_duration_ms, 2.0);
        assert_eq!(config.tx_deadline_ms, 5.0);
        assert_eq!(config.grid_block_samples().unwrap(), 19_440);
        let smaller = CompositeConfig {
            block_duration_ms: 5.0 / 6.0,
            tx_deadline_ms: 2.0,
            ..config
        };
        smaller.validate().unwrap();
        assert_eq!(smaller.grid_block_samples().unwrap(), 8_100);
        for duration in [0.0, -1.0, f64::NAN, f64::INFINITY, MAX_TIMING_MS + 1.0] {
            assert!(
                CompositeConfig {
                    block_duration_ms: duration,
                    ..config
                }
                .validate()
                .is_err()
            );
        }
        for duration in [-1.0, f64::NAN, f64::INFINITY, MAX_TIMING_MS + 1.0] {
            assert!(
                CompositeConfig {
                    tx_deadline_ms: duration,
                    ..config
                }
                .validate()
                .is_err()
            );
        }
    }

    #[test]
    fn a_block_under_one_sample_or_past_the_limit_is_refused() {
        let tiny = CompositeConfig {
            device_rate_hz: 1_000,
            block_duration_ms: 0.1,
            ..PLAN
        };
        assert!(
            tiny.validate()
                .unwrap_err()
                .to_string()
                .contains("under one sample")
        );
        let huge = CompositeConfig {
            device_rate_hz: 61_440_000,
            block_duration_ms: MAX_TIMING_MS,
            ..PLAN
        };
        assert!(
            huge.validate()
                .unwrap_err()
                .to_string()
                .contains("past the")
        );
    }

    #[test]
    fn a_plan_without_a_rate_or_center_is_refused() {
        PLAN.validate().unwrap();
        for broken in [
            CompositeConfig {
                device_rate_hz: 0,
                ..PLAN
            },
            CompositeConfig {
                tx_center_hz: 0,
                ..PLAN
            },
            CompositeConfig {
                rx_center_hz: 0,
                ..PLAN
            },
            CompositeConfig {
                tx_digital_backoff_db: 3.0,
                ..PLAN
            },
            CompositeConfig {
                tx_digital_backoff_db: f64::NAN,
                ..PLAN
            },
        ] {
            assert!(broken.validate().is_err(), "{broken:?}");
        }
    }

    #[test]
    fn a_device_is_checked_against_the_plan_by_what_it_applied() {
        use crate::config::RadioConfig;

        // A noop radio clocks in nanoseconds, and 1 GHz is not a whole multiple of
        // 3.888 MHz, so the plan is refused with the clock named.
        let radio = RadioConfig {
            tx_sample_rate_hz: 3_888_000,
            ..RadioConfig::default()
        }
        .open()
        .expect("a noop radio opens");
        let message = PLAN
            .verify_device(radio.as_ref())
            .expect_err("a 1 GHz clock cannot divide 3.888 MHz")
            .to_string();
        assert!(message.contains("not a whole multiple"), "{message}");
        assert!(message.contains("master_clock_rate"), "{message}");

        // A rate that same clock does divide passes.
        let radio = RadioConfig {
            tx_sample_rate_hz: 4_000_000,
            ..RadioConfig::default()
        }
        .open()
        .expect("a noop radio opens");
        let plan = CompositeConfig {
            device_rate_hz: 4_000_000,
            ..PLAN
        };
        plan.verify_device(radio.as_ref()).unwrap();
    }

    #[test]
    fn a_device_that_came_up_at_another_rate_is_refused() {
        use crate::config::RadioConfig;

        // The device is at 4 MHz but the plan asks for 3.888 MHz, which is what a
        // driver coercing the rate looks like from outside.
        let radio = RadioConfig {
            tx_sample_rate_hz: 4_000_000,
            ..RadioConfig::default()
        }
        .open()
        .expect("a noop radio opens");
        let message = PLAN
            .verify_device(radio.as_ref())
            .expect_err("a coerced rate is refused")
            .to_string();
        assert!(message.contains("move every carrier offset"), "{message}");
    }

    #[test]
    fn backoff_converts_to_a_linear_amplitude() {
        assert!((PLAN.tx_scale() - 0.251_188_6).abs() < 1e-6);
        let unity = CompositeConfig {
            tx_digital_backoff_db: 0.0,
            ..PLAN
        };
        assert!((unity.tx_scale() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_fractional_rate_matches_the_device_fraction_it_lands_on() {
        let rate_hz = 3_250_000.0 / 3.0;
        for (device_rate_hz, terms) in [(4_875_000, (9, 2)), (13_000_000, (12, 1))] {
            let config = CompositeConfig {
                device_rate_hz,
                ..PLAN
            };
            assert_eq!(
                config.rate_terms(rate_hz).unwrap(),
                terms,
                "{device_rate_hz} Hz"
            );
            let exact = config.exact_client_rate_hz(rate_hz).unwrap();
            assert!((exact - rate_hz).abs() < RATE_MATCH_TOLERANCE_HZ);
        }
        assert!(
            PLAN.rate_ratio(rate_hz).is_err(),
            "5832/1625 is past the ratio limit"
        );
        let config = CompositeConfig {
            device_rate_hz: 4_875_000,
            ..PLAN
        };
        assert!(config.rate_ratio(rate_hz + 0.25).is_err());
    }

    #[test]
    fn both_projects_native_rates_divide_the_composite() {
        assert_eq!(PLAN.rate_ratio(388_800.0).unwrap(), 10);
        assert_eq!(PLAN.rate_ratio(3_888_000.0).unwrap(), 1);
        assert_eq!(PLAN.rate_ratio(1_944_000.0).unwrap(), 2);
    }

    #[test]
    fn rational_rates_use_the_reduced_lattice() {
        assert_eq!(PLAN.rate_ratio(100_000.0).unwrap(), 972);
        let plan = PLAN
            .plan_slice(Direction::Tx, 100_000.0, PLAN.tx_center_hz, Some(30_000))
            .unwrap();
        assert_eq!(plan.denominator, 25);
    }

    #[test]
    fn a_rate_above_the_device_rate_or_at_zero_is_refused() {
        assert!(PLAN.rate_ratio(0.0).is_err());
        let message = PLAN.rate_ratio(4_000_000.0).unwrap_err().to_string();
        assert!(message.contains("exceeds"), "{message}");
    }

    #[test]
    fn a_ratio_past_the_filter_limit_is_refused() {
        let wide = CompositeConfig {
            device_rate_hz: 1 << 20,
            ..PLAN
        };
        assert_eq!(wide.rate_ratio((1 << 8) as f64).unwrap(), 4096);
        let message = wide.rate_ratio((1 << 7) as f64).unwrap_err().to_string();
        assert!(message.contains("past the 4096 limit"), "{message}");
    }

    #[test]
    fn the_configured_carriers_land_at_the_expected_offsets() {
        assert_eq!(
            PLAN.offset_hz(Direction::Tx, CLIENT_RATE_HZ as f64, 866_000_000)
                .unwrap(),
            -1_520_000
        );
        assert_eq!(
            PLAN.offset_hz(Direction::Tx, CLIENT_RATE_HZ as f64, 869_040_000)
                .unwrap(),
            1_520_000
        );
        assert_eq!(
            PLAN.offset_hz(Direction::Rx, CLIENT_RATE_HZ as f64, 821_000_000)
                .unwrap(),
            -1_520_000
        );
        assert_eq!(
            PLAN.offset_hz(Direction::Rx, CLIENT_RATE_HZ as f64, 824_040_000)
                .unwrap(),
            1_520_000
        );
        assert_eq!(
            PLAN.offset_hz(Direction::Tx, CLIENT_RATE_HZ as f64, PLAN.tx_center_hz)
                .unwrap(),
            0
        );
    }

    #[test]
    fn a_carrier_whose_own_band_reaches_past_the_edge_is_refused() {
        // 1,520,000 + 194,400 fits inside the 1,944,000 Hz half-span.
        PLAN.offset_hz(Direction::Tx, CLIENT_RATE_HZ as f64, 869_040_000)
            .unwrap();
        // The same carrier at the full device rate reaches 1,520,000 + 1,944,000.
        let message = PLAN
            .offset_hz(Direction::Tx, 3_888_000.0, 869_040_000)
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("beyond the 1944000 Hz half-span"),
            "{message}"
        );
        // A carrier past the edge outright.
        assert!(
            PLAN.offset_hz(Direction::Rx, CLIENT_RATE_HZ as f64, 825_000_000)
                .is_err()
        );
        assert!(
            PLAN.offset_hz(Direction::Tx, CLIENT_RATE_HZ as f64, 0)
                .is_err()
        );
    }

    #[test]
    fn the_check_tightens_for_a_client_that_samples_slowly() {
        // A slow client fits nearer the edge than a fast one, with no guard constant.
        let edge_hz = PLAN.tx_center_hz + 1_900_000;
        PLAN.offset_hz(Direction::Tx, 48_000.0, edge_hz).unwrap();
        assert!(PLAN.offset_hz(Direction::Tx, 388_800.0, edge_hz).is_err());
    }

    #[test]
    fn a_slice_plan_states_the_ratio_offset_and_filter() {
        let plan = PLAN
            .plan_slice(
                Direction::Rx,
                CLIENT_RATE_HZ as f64,
                821_000_000,
                Some(20_000),
            )
            .unwrap();
        assert_eq!(plan.ratio, 10);
        assert_eq!(plan.offset_hz, -1_520_000);
        assert_eq!(plan.taps.len() % 2, 1);
        assert!(plan.taps.len() > 3);
        assert!((plan.taps.iter().sum::<f64>() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn a_narrow_client_bandwidth_buys_a_shorter_filter() {
        let narrow = PLAN
            .plan_slice(
                Direction::Rx,
                CLIENT_RATE_HZ as f64,
                821_000_000,
                Some(20_000),
            )
            .unwrap();
        let wide = PLAN
            .plan_slice(Direction::Rx, CLIENT_RATE_HZ as f64, 821_000_000, None)
            .unwrap();
        assert!(
            wide.taps.len() > narrow.taps.len(),
            "{} vs {}",
            wide.taps.len(),
            narrow.taps.len()
        );
    }

    #[test]
    fn a_bandwidth_wider_than_the_client_band_is_clamped_not_refused() {
        // A client asking for more than its own Nyquist cannot have it, and asking is
        // not an error: the slice is simply its whole usable band.
        let absurd = PLAN
            .plan_slice(
                Direction::Rx,
                CLIENT_RATE_HZ as f64,
                821_000_000,
                Some(10_000_000),
            )
            .unwrap();
        let defaulted = PLAN
            .plan_slice(Direction::Rx, CLIENT_RATE_HZ as f64, 821_000_000, None)
            .unwrap();
        assert_eq!(absurd.taps, defaulted.taps);
    }

    #[test]
    fn a_client_at_the_device_rate_needs_no_filter() {
        let plan = PLAN
            .plan_slice(Direction::Tx, 3_888_000.0, PLAN.tx_center_hz, None)
            .unwrap();
        assert_eq!(plan.ratio, 1);
        assert_eq!(plan.taps, vec![1.0]);
    }

    #[test]
    fn a_zero_bandwidth_still_designs_a_usable_filter() {
        let plan = PLAN
            .plan_slice(Direction::Rx, CLIENT_RATE_HZ as f64, 821_000_000, Some(0))
            .unwrap();
        assert_eq!(plan.taps.len() % 2, 1);
        assert!((plan.taps.iter().sum::<f64>() - 1.0).abs() < 1e-12);
    }
}
