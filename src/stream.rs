use std::time::Duration;

use crate::error::Error;

/// Device-clock ticks. The clock origin is supplied by the owning stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceTime(u64);
impl DeviceTime {
    /// Uses ticks from the stream's clock, not Unix time or nanoseconds unless its clock uses them.
    pub const fn from_ticks(ticks: u64) -> Self {
        Self(ticks)
    }
    /// Raw ticks for a native driver using the same clock.
    pub const fn ticks(self) -> u64 {
        self.0
    }
}

/// When the first sample of a write should appear on the stream timeline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Schedule {
    /// Submit without a timestamp.
    #[default]
    Immediate,
    /// Schedule against the owning TX stream's device clock.
    At(DeviceTime),
}
impl Schedule {
    pub(crate) fn ticks(self) -> Option<u64> {
        match self {
            Self::Immediate => None,
            Self::At(time) => Some(time.ticks()),
        }
    }
}

/// Metadata for a TX chunk. Burst boundaries do not tear down the native stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxMetadata {
    /// Time of the first sample in this chunk.
    pub schedule: Schedule,
    /// Marks the final samples of a burst. Later bursts reuse the same stream.
    pub end_of_burst: bool,
}

/// Independent TX operations. Scheduling samples does not imply scheduled activation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TxCapability {
    /// Samples can be scheduled against the device or virtual sample clock.
    TimedSamples,
    /// The stream can start at a requested timestamp.
    TimedActivation,
    /// The stream clock can be reset to a supplied value.
    ClockReset,
    /// Final-sample metadata completes a burst while keeping the stream ready.
    BurstEnd,
}

/// A set of independently supported TX operations. The default set is empty.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct TxCapabilities {
    bits: u8,
}
impl TxCapabilities {
    /// Adds an operation without changing the other members of the set.
    pub const fn with(self, capability: TxCapability) -> Self {
        Self {
            bits: self.bits | (1 << capability as u8),
        }
    }
    /// Whether the driver advertises support for this operation.
    pub const fn supports(self, capability: TxCapability) -> bool {
        self.bits & (1 << capability as u8) != 0
    }
    /// Returns each advertised operation once.
    pub fn iter(self) -> impl Iterator<Item = TxCapability> {
        [
            TxCapability::TimedSamples,
            TxCapability::TimedActivation,
            TxCapability::ClockReset,
            TxCapability::BurstEnd,
        ]
        .into_iter()
        .filter(move |capability| self.supports(*capability))
    }
}
impl std::fmt::Debug for TxCapabilities {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}
impl FromIterator<TxCapability> for TxCapabilities {
    fn from_iter<T: IntoIterator<Item = TxCapability>>(iter: T) -> Self {
        iter.into_iter().fold(Self::default(), Self::with)
    }
}
impl<const N: usize> From<[TxCapability; N]> for TxCapabilities {
    fn from(capabilities: [TxCapability; N]) -> Self {
        capabilities.into_iter().collect()
    }
}

/// A receive wait request. Unsupported modes return an error rather than changing meaning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadTimeout {
    /// Return immediately if no samples are available.
    NonBlocking,
    /// Wait for at most this driver budget. Operating-system scheduling can add delay.
    For(Duration),
    /// Wait until samples or a stream failure are available.
    Forever,
}
impl ReadTimeout {
    pub(crate) fn microseconds(self) -> Result<i64, Error> {
        match self {
            Self::NonBlocking => Ok(0),
            Self::Forever => Ok(-1),
            Self::For(duration) => i64::try_from(duration.as_nanos().div_ceil(1000))
                .map_err(|_| Error::invalid_setting("receive timeout exceeds i64 microseconds")),
        }
    }
}

pub(crate) fn positive_hz(value: f64, name: &str) -> Result<f64, Error> {
    if !value.is_finite() || value <= 0.0 {
        return Err(Error::invalid_setting(format!(
            "{name} must be finite and positive, got {value}"
        )));
    }
    Ok(value)
}

pub(crate) fn finite(value: f64, name: &str) -> Result<f64, Error> {
    if !value.is_finite() {
        return Err(Error::invalid_setting(format!(
            "{name} must be finite, got {value}"
        )));
    }
    Ok(value)
}

#[cfg(feature = "lime-backend")]
pub(crate) fn whole_u32(value: f64, name: &str) -> Result<u32, Error> {
    finite(value, name)?;
    if value < 0.0 || value > u32::MAX as f64 || value.fract() != 0.0 {
        return Err(Error::invalid_setting(format!(
            "{name} requires a whole value within u32"
        )));
    }
    Ok(value as u32)
}
#[cfg(feature = "bladerf-backend")]
pub(crate) fn whole_i32(value: f64, name: &str) -> Result<i32, Error> {
    finite(value, name)?;
    if value < i32::MIN as f64 || value > i32::MAX as f64 || value.fract() != 0.0 {
        return Err(Error::invalid_setting(format!(
            "{name} requires a whole value within i32"
        )));
    }
    Ok(value as i32)
}
#[cfg(feature = "bladerf-backend")]
pub(crate) fn frequency_u64(value: f64) -> Result<u64, Error> {
    positive_hz(value, "frequency")?;
    if value >= u64::MAX as f64 || value.fract() != 0.0 {
        return Err(Error::invalid_setting(
            "frequency requires whole Hz within u64",
        ));
    }
    Ok(value as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wait_modes_keep_their_meaning_and_submicrosecond_waits_round_up() {
        assert_eq!(ReadTimeout::NonBlocking.microseconds().unwrap(), 0);
        assert_eq!(ReadTimeout::Forever.microseconds().unwrap(), -1);
        assert_eq!(
            ReadTimeout::For(Duration::from_nanos(1))
                .microseconds()
                .unwrap(),
            1
        );
        assert!(ReadTimeout::For(Duration::MAX).microseconds().is_err());
    }
    #[test]
    fn capability_sets_deduplicate_members_and_start_empty() {
        let empty = TxCapabilities::default();
        assert_eq!(empty.iter().count(), 0);
        let capabilities = empty
            .with(TxCapability::TimedSamples)
            .with(TxCapability::BurstEnd)
            .with(TxCapability::TimedSamples);
        assert_eq!(
            capabilities.iter().collect::<Vec<_>>(),
            [TxCapability::TimedSamples, TxCapability::BurstEnd]
        );
        assert!(!capabilities.supports(TxCapability::TimedActivation));
        assert!(!capabilities.supports(TxCapability::ClockReset));
    }
}
