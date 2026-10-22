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
