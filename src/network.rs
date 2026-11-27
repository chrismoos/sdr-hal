/// Locally extrapolated remote time with bounded corrections. Requires `network-backend`.
#[cfg(feature = "network-backend")]
pub(crate) mod clock;
/// Version 3 IQ packet encoding shared by client and server. Available without network runtime features.
pub mod wire;

#[cfg(feature = "network-backend")]
mod client;
/// Generated control messages and gRPC client/server interfaces. All hardware timestamps are nanoseconds.
#[cfg(feature = "network-backend")]
pub mod proto {
    tonic::include_proto!("radio.v1");
}
#[cfg(feature = "network-backend")]
pub use client::{AcceptedRanges, NetworkRadio, NetworkRadioOptions};

#[cfg(any(feature = "daemon", all(test, feature = "network-backend")))]
pub(crate) fn encode_capabilities(tx: crate::TxCapabilities, receive: bool) -> Vec<i32> {
    let mut values: Vec<_> = tx
        .iter()
        .map(|capability| match capability {
            crate::TxCapability::TimedSamples => proto::RadioCapability::TimedTxSamples as i32,
            crate::TxCapability::TimedActivation => {
                proto::RadioCapability::TimedTxActivation as i32
            }
            crate::TxCapability::ClockReset => proto::RadioCapability::ClockReset as i32,
            crate::TxCapability::BurstEnd => proto::RadioCapability::TxBurstEnd as i32,
        })
        .collect();
    if receive {
        values.push(proto::RadioCapability::Receive as i32);
    }
    values
}
#[cfg(feature = "network-backend")]
pub(crate) fn decode_tx_capabilities(values: &[i32]) -> crate::TxCapabilities {
    values
        .iter()
        .filter_map(
            |value| match proto::RadioCapability::try_from(*value).ok()? {
                proto::RadioCapability::TimedTxSamples => Some(crate::TxCapability::TimedSamples),
                proto::RadioCapability::TimedTxActivation => {
                    Some(crate::TxCapability::TimedActivation)
                }
                proto::RadioCapability::ClockReset => Some(crate::TxCapability::ClockReset),
                proto::RadioCapability::TxBurstEnd => Some(crate::TxCapability::BurstEnd),
                proto::RadioCapability::Unspecified | proto::RadioCapability::Receive => None,
            },
        )
        .collect()
}

#[cfg(feature = "daemon")]
pub(crate) fn encode_time_reference(reference: crate::TimeReference) -> proto::TimeReference {
    proto::TimeReference {
        device_ticks: reference.device_ticks,
        seconds: reference.seconds,
        subsecond_ns: reference.subsecond_ns,
        leap_seconds: u32::from(reference.leap_seconds),
        source: match reference.source {
            crate::ReferenceSource::HostClock => proto::TimeReferenceSource::HostClock,
            crate::ReferenceSource::Gps => proto::TimeReferenceSource::Gps,
        } as i32,
        lock: match reference.lock {
            crate::ReferenceLock::Locked => proto::TimeReferenceLock::Locked,
            crate::ReferenceLock::Holdover => proto::TimeReferenceLock::Holdover,
            crate::ReferenceLock::Unlocked => proto::TimeReferenceLock::Unlocked,
        } as i32,
        uncertainty_ns: reference.uncertainty_ns,
    }
}

#[cfg(feature = "network-backend")]
pub(crate) fn decode_time_reference(
    reference: &proto::TimeReference,
) -> Result<crate::TimeReference, crate::error::Error> {
    use crate::error::Error;
    let source = match proto::TimeReferenceSource::try_from(reference.source) {
        Ok(proto::TimeReferenceSource::HostClock) => crate::ReferenceSource::HostClock,
        Ok(proto::TimeReferenceSource::Gps) => crate::ReferenceSource::Gps,
        _ => {
            return Err(Error::invalid_setting(format!(
                "time reference source {} is not known",
                reference.source
            )));
        }
    };
    let lock = match proto::TimeReferenceLock::try_from(reference.lock) {
        Ok(proto::TimeReferenceLock::Locked) => crate::ReferenceLock::Locked,
        Ok(proto::TimeReferenceLock::Holdover) => crate::ReferenceLock::Holdover,
        Ok(proto::TimeReferenceLock::Unlocked) => crate::ReferenceLock::Unlocked,
        _ => {
            return Err(Error::invalid_setting(format!(
                "time reference lock {} is not known",
                reference.lock
            )));
        }
    };
    let leap_seconds = u8::try_from(reference.leap_seconds).map_err(|_| {
        Error::invalid_setting(format!(
            "time reference leap seconds {} exceed 255",
            reference.leap_seconds
        ))
    })?;
    Ok(crate::TimeReference {
        device_ticks: reference.device_ticks,
        seconds: reference.seconds,
        subsecond_ns: reference.subsecond_ns,
        leap_seconds,
        source,
        lock,
        uncertainty_ns: reference.uncertainty_ns,
    })
}

#[cfg(all(test, feature = "daemon"))]
mod time_reference_tests {
    use super::*;

    #[test]
    fn time_reference_survives_the_wire() {
        let reference = crate::TimeReference {
            device_ticks: 39_321_600_123,
            seconds: 1_479_000_000,
            subsecond_ns: 999_999_999,
            leap_seconds: 18,
            source: crate::ReferenceSource::Gps,
            lock: crate::ReferenceLock::Holdover,
            uncertainty_ns: 40,
        };
        assert_eq!(
            decode_time_reference(&encode_time_reference(reference)).unwrap(),
            reference
        );
    }

    #[test]
    fn unknown_time_reference_source_is_rejected() {
        let mut encoded = encode_time_reference(crate::TimeReference {
            device_ticks: 0,
            seconds: 0,
            subsecond_ns: 0,
            leap_seconds: 0,
            source: crate::ReferenceSource::HostClock,
            lock: crate::ReferenceLock::Locked,
            uncertainty_ns: 0,
        });
        encoded.source = proto::TimeReferenceSource::Unspecified as i32;
        assert!(decode_time_reference(&encoded).is_err());
    }
}

#[cfg(all(test, feature = "network-backend"))]
mod capability_tests {
    use super::*;
    #[test]
    fn wire_capability_sets_keep_independent_operations_and_ignore_unknown_entries() {
        let tx = crate::TxCapabilities::from([
            crate::TxCapability::TimedSamples,
            crate::TxCapability::BurstEnd,
        ]);
        let mut encoded = encode_capabilities(tx, true);
        assert!(encoded.contains(&(proto::RadioCapability::Receive as i32)));
        encoded.push(i32::MAX);
        encoded.push(proto::RadioCapability::Unspecified as i32);
        encoded.push(proto::RadioCapability::TimedTxSamples as i32);
        assert_eq!(decode_tx_capabilities(&encoded), tx);
        assert!(!tx.supports(crate::TxCapability::TimedActivation));
        assert!(!tx.supports(crate::TxCapability::ClockReset));
    }
}
