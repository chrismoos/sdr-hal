use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::error::Error;

/// 1980-01-06T00:00:00Z as Unix seconds, the epoch [`TimeReference::seconds`] counts from.
const REFERENCE_EPOCH_UNIX_SECONDS: u64 = 315_964_800;
const HOST_CLOCK_READS: usize = 8;

/// The GPS hardware a radio has, which selects how its time is used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gps {
    /// A GPS-disciplined oscillator, as from a GPSDO module or a GPS-locked 10 MHz
    /// reference on the `gpsdo` or `external` clock source. The PPS gives the time of
    /// each tick, and samples and frequencies pass through unchanged.
    Gpsdo,
    /// A GPS receiver's PPS with a free-running oscillator. The PPS gives the time of
    /// each tick and the oscillator error, which is corrected by resampling the
    /// streams and shifting both carrier frequencies.
    Pps,
}

/// Where a [`TimeReference`] got its time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferenceSource {
    /// The host's UTC clock, paired with a device clock read.
    HostClock,
    /// A GPS receiver and its PPS.
    Gps,
}

/// Whether the source of a [`TimeReference`] is locked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferenceLock {
    /// The source is locked.
    Locked,
    /// The source lost lock and the device clock is free-running.
    Holdover,
    /// The source has never locked.
    Unlocked,
}

/// State of a clock disciplined to an external reference.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClockStatus {
    /// Lock state of the reference.
    pub lock: ReferenceLock,
    /// Rate of the device oscillator relative to the reference, in parts per billion.
    /// Positive means the oscillator runs fast.
    pub rate_error_ppb: f64,
    /// How far the last reference pulse was from its expected time, in nanoseconds.
    pub phase_error_ns: f64,
    /// Reference pulses accepted since tracking started.
    pub accepted_pulses: u64,
    /// Reference pulses rejected as too far from their expected time.
    pub rejected_pulses: u64,
    /// Whether the reference receiver reports a fix.
    pub receiver_fix: bool,
    /// Satellites the reference receiver uses, when it reports the count.
    pub satellites: Option<u8>,
    /// The time of the receiver's most recent report, in Unix seconds.
    pub receiver_unix_seconds: Option<f64>,
    /// The last position the receiver reported with a fix.
    pub position: Option<ReceiverPosition>,
    /// Horizontal dilution of precision of the receiver's fix.
    pub hdop: Option<f64>,
    /// Whether the clock loop has settled on its slow gains.
    pub settled: bool,
    /// Spread of the applied rate correction around its trend over the last minute, in
    /// parts per billion RMS. It estimates how steady the corrected frequency is, and
    /// says nothing about an error of the reference itself.
    pub rate_stability_ppb: Option<f64>,
}

/// Where a reference receiver is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReceiverPosition {
    /// Degrees north of the equator.
    pub latitude_deg: f64,
    /// Degrees east of the prime meridian.
    pub longitude_deg: f64,
    /// Meters above mean sea level.
    pub altitude_m: Option<f64>,
}

/// The time at one device clock tick.
///
/// A host-clock reference reports UTC seconds with zero leap seconds. So does a GPS
/// reference from a disciplined radio, which takes UTC from the receiver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeReference {
    /// Device clock value the time below applies to.
    pub device_ticks: u64,
    /// Whole seconds since 1980-01-06T00:00:00 at `device_ticks`.
    pub seconds: u64,
    /// Nanoseconds past `seconds`.
    pub subsecond_ns: u32,
    /// The reported time minus UTC, in seconds.
    pub leap_seconds: u8,
    /// Where the time came from.
    pub source: ReferenceSource,
    /// Lock state of the source.
    pub lock: ReferenceLock,
    /// Half the width of the interval the true time lies in, in nanoseconds.
    pub uncertainty_ns: u64,
}

impl TimeReference {
    /// Pairs `device_ticks`, read over `round_trip`, with the host's UTC clock now.
    pub(crate) fn host_clock_pair(device_ticks: u64, round_trip: Duration) -> Result<Self, Error> {
        let utc = SystemTime::now()
            .checked_sub(round_trip / 2)
            .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
            .and_then(|unix| unix.checked_sub(Duration::from_secs(REFERENCE_EPOCH_UNIX_SECONDS)))
            .ok_or_else(|| Error::invalid_setting("host clock is before 1980-01-06"))?;
        Ok(Self {
            device_ticks,
            seconds: utc.as_secs(),
            subsecond_ns: utc.subsec_nanos(),
            leap_seconds: 0,
            source: ReferenceSource::HostClock,
            lock: ReferenceLock::Locked,
            uncertainty_ns: (round_trip.as_nanos() / 2) as u64,
        })
    }

    /// Pairs the host's UTC clock with a device clock read. Reads the device several
    /// times and keeps the read with the shortest round trip.
    pub fn from_host_clock(
        mut read_ticks: impl FnMut() -> Result<u64, Error>,
    ) -> Result<Self, Error> {
        let mut best: Option<(Self, Duration)> = None;
        let mut last_error = None;
        for _ in 0..HOST_CLOCK_READS {
            let started = Instant::now();
            let device_ticks = match read_ticks() {
                Ok(ticks) => ticks,
                Err(error) => {
                    last_error = Some(error);
                    continue;
                }
            };
            let round_trip = started.elapsed();
            if best
                .as_ref()
                .is_some_and(|(_, best_trip)| round_trip >= *best_trip)
            {
                continue;
            }
            best = Some((Self::host_clock_pair(device_ticks, round_trip)?, round_trip));
        }
        match (best, last_error) {
            (Some((reference, _)), _) => Ok(reference),
            (None, Some(error)) => Err(error),
            (None, None) => Err(Error::invalid_setting("no device clock read was attempted")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_clock_reference_reports_utc_seconds_with_zero_leap() {
        let reference = TimeReference::from_host_clock(|| Ok(1234)).unwrap();
        let utc_seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - REFERENCE_EPOCH_UNIX_SECONDS;
        assert_eq!(reference.device_ticks, 1234);
        assert!(reference.seconds.abs_diff(utc_seconds) <= 1);
        assert_eq!(reference.leap_seconds, 0);
        assert_eq!(reference.source, ReferenceSource::HostClock);
        assert_eq!(reference.lock, ReferenceLock::Locked);
    }

    #[test]
    fn host_clock_reference_survives_some_failed_reads() {
        let mut calls = 0;
        let reference = TimeReference::from_host_clock(|| {
            calls += 1;
            if calls % 2 == 0 {
                Ok(calls)
            } else {
                Err(Error::unsupported("device time"))
            }
        })
        .unwrap();
        assert!(reference.device_ticks >= 2);
    }

    #[test]
    fn host_clock_reference_reports_the_error_when_every_read_fails() {
        let error =
            TimeReference::from_host_clock(|| Err(Error::unsupported("device time"))).unwrap_err();
        assert_eq!(error.kind(), crate::error::ErrorKind::Unsupported);
    }
}
