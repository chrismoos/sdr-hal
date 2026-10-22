//! Picks one rational step or a rational step plus an integer step, by multiply-accumulates.
//! `factor` divides the slice ratio, so every grid block holds whole intermediate samples.

use super::Direction;
use super::resample::{self, MAX_TAPS_PER_PHASE};
use crate::dsp::design::{self, MAX_LOWPASS_TAPS};
use crate::error::Error;

const POLYPHASE_ROW_ALIGN: usize = 4;
/// Symmetric taps are folded, so a decimator multiplies by half of them.
const SYMMETRIC_TAP_FOLD: usize = 2;

/// Integer step between the rational step and the device rate.
#[derive(Clone, Debug, PartialEq)]
pub struct IntermediateStage {
    /// Device samples per intermediate sample.
    pub factor: usize,
    /// Low-pass taps at the device rate.
    pub taps: Vec<f64>,
}

impl IntermediateStage {
    pub(super) fn rate_hz(&self, device_rate_hz: u64) -> f64 {
        device_rate_hz as f64 / self.factor as f64
    }
}

struct Candidate {
    stage: Option<IntermediateStage>,
    cost: f64,
}

/// Returns the cheapest conversion, `None` meaning one rational step. The client rate is
/// `device_rate_hz * denominator / ratio`, with the fraction reduced.
pub(super) fn choose(
    direction: Direction,
    device_rate_hz: u64,
    ratio: usize,
    denominator: usize,
    passband_hz: f64,
) -> Result<Option<IntermediateStage>, Error> {
    let device = device_rate_hz as f64;
    let client = device * denominator as f64 / ratio as f64;
    let single = rational_taps(device, client, passband_hz).map(|taps| Candidate {
        stage: None,
        cost: match direction {
            Direction::Tx => device * taps as f64,
            Direction::Rx => client * taps as f64,
        },
    });
    let best = (2..=ratio)
        .filter(|factor| ratio.is_multiple_of(*factor))
        .filter_map(|factor| two_stage(direction, device, client, factor, passband_hz))
        .chain(single)
        .min_by(|a, b| a.cost.total_cmp(&b.cost))
        .ok_or_else(|| -> Error {
            format!(
                "client rate {client} Hz needs a rational filter longer than {MAX_TAPS_PER_PHASE} taps per phase"
            )
            .into()
        })?;
    Ok(best.stage)
}

fn rational_taps(input_hz: f64, output_hz: f64, passband_hz: f64) -> Option<usize> {
    if input_hz.min(output_hz) / 2.0 <= passband_hz {
        return None;
    }
    let taps = resample::tap_count(input_hz, output_hz, passband_hz);
    (taps <= MAX_TAPS_PER_PHASE).then_some(taps)
}

/// `factor` divides the ratio, so the rational step's own terms are
/// `(ratio / factor, denominator)` and stay within the ratio limit.
fn two_stage(
    direction: Direction,
    device: f64,
    client: f64,
    factor: usize,
    passband_hz: f64,
) -> Option<Candidate> {
    let mid = device / factor as f64;
    let rational = match direction {
        Direction::Tx => rational_taps(client, mid, passband_hz)?,
        Direction::Rx => rational_taps(mid, client, passband_hz)?,
    };
    let taps = integer_taps(device, client, mid, passband_hz)?;
    let cost = match direction {
        Direction::Tx => {
            mid * rational as f64
                + device
                    * taps
                        .len()
                        .div_ceil(factor)
                        .next_multiple_of(POLYPHASE_ROW_ALIGN) as f64
        }
        Direction::Rx => {
            mid * taps.len().div_ceil(SYMMETRIC_TAP_FOLD) as f64 + client * rational as f64
        }
    };
    Some(Candidate {
        stage: Some(IntermediateStage { factor, taps }),
        cost,
    })
}

/// The stopband starts at the nearest image or alias the rational step leaves,
/// `mid - min(client, mid) / 2`.
fn integer_taps(device: f64, client: f64, mid: f64, passband_hz: f64) -> Option<Vec<f64>> {
    let stopband_hz = mid - client.min(mid) / 2.0;
    if stopband_hz <= passband_hz || stopband_hz / device > 0.5 {
        return None;
    }
    let cutoff = (passband_hz + stopband_hz) / 2.0 / device;
    let count = design::taps_for_transition(stopband_hz / device - cutoff);
    (count < MAX_LOWPASS_TAPS).then(|| design::blackman_sinc_lowpass(count, cutoff))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE_RATE: u64 = 12_288_000;

    fn mid_rate(direction: Direction, terms: (usize, usize), passband: f64) -> Option<f64> {
        choose(direction, DEVICE_RATE, terms.0, terms.1, passband)
            .unwrap()
            .map(|stage| stage.rate_hz(DEVICE_RATE))
    }

    #[test]
    fn narrow_clients_convert_through_an_intermediate_rate() {
        // 12.288 MHz is 384/25 of 800 kHz and 2560/81 of 388.8 kHz.
        for (terms, passband) in [((384, 25), 100_000.0), ((2560, 81), 15_000.0)] {
            for direction in [Direction::Tx, Direction::Rx] {
                let mid = mid_rate(direction, terms, passband)
                    .unwrap_or_else(|| panic!("{terms:?} {direction:?} stayed single-stage"));
                assert!(
                    mid < DEVICE_RATE as f64 / 2.0,
                    "{terms:?} {direction:?} chose {mid} Hz"
                );
            }
        }
    }

    #[test]
    fn every_chosen_factor_divides_the_slice_ratio() {
        for (ratio, denominator, passband) in [
            (5, 2, 625_000.0),
            (384, 25, 320_000.0),
            (2560, 81, 15_000.0),
        ] {
            for direction in [Direction::Tx, Direction::Rx] {
                if let Some(stage) =
                    choose(direction, DEVICE_RATE, ratio, denominator, passband).unwrap()
                {
                    assert!(
                        ratio.is_multiple_of(stage.factor),
                        "{ratio}/{denominator}: factor {}",
                        stage.factor
                    );
                    assert_eq!(stage.taps.len() % 2, 1);
                }
            }
        }
    }

    #[test]
    fn a_client_too_narrow_for_any_filter_is_refused() {
        assert!(choose(Direction::Rx, DEVICE_RATE, 4096, 1, 1_499.9).is_err());
    }
}
