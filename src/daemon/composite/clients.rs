//! Per-client control-plane state in composite mode.
//!
//! A client configures its rate, carrier and bandwidth before it has a session token, so
//! the control connection is what identifies it. Settings are recorded as they arrive and
//! checked against each other as soon as both halves of a check are known, then checked
//! again in full when the client opens its session.

use std::collections::HashMap;
use std::net::SocketAddr;

use super::{CompositeConfig, Direction, SlicePlan};
use crate::error::Error;

/// What one client asked for in one direction.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct StreamRequest {
    pub(crate) sample_rate_hz: Option<f64>,
    pub(crate) carrier_hz: Option<u64>,
    pub(crate) bandwidth_hz: Option<u64>,
}

impl StreamRequest {
    /// Checks whatever is already known, so a mistake is reported at the call that made
    /// it rather than at session open.
    fn check(&self, config: &CompositeConfig, direction: Direction) -> Result<(), Error> {
        if let Some(rate) = self.sample_rate_hz {
            config.rate_ratio(rate)?;
        }
        if let (Some(rate), Some(carrier)) = (self.sample_rate_hz, self.carrier_hz) {
            config.offset_hz(direction, rate, carrier)?;
        }
        Ok(())
    }

    fn plan(&self, config: &CompositeConfig, direction: Direction) -> Result<SlicePlan, Error> {
        let rate = self.sample_rate_hz.ok_or_else(|| -> Error {
            format!(
                "a composite {} stream needs a sample rate before its session opens",
                direction.label()
            )
            .into()
        })?;
        // A client tunes after it splits, so a session may open before its carrier is
        // known. The slice then sits at the center until the retune arrives, which is
        // harmless because nothing transmits until the client enables it.
        let carrier = self.carrier_hz.unwrap_or(match direction {
            Direction::Tx => config.tx_center_hz,
            Direction::Rx => config.rx_center_hz,
        });
        config.plan_slice(direction, rate, carrier, self.bandwidth_hz)
    }
}

/// One client's accumulated settings.
#[derive(Clone, Debug, Default)]
pub(crate) struct ClientSettings {
    pub(crate) tx: StreamRequest,
    pub(crate) rx: StreamRequest,
    /// Set once the client opens a session.
    pub(crate) session_id: Option<u32>,
    pub(crate) rx_configured: bool,
    /// Grid slot, assigned when the session opens.
    pub(crate) tx_slot: Option<usize>,
    /// Fan-out slot, assigned when the session opens.
    pub(crate) rx_slot: Option<usize>,
    /// Whether this client asked its receiver to run.
    pub(crate) rx_active: bool,
}

impl ClientSettings {
    /// A client transmits when it configured a TX rate, which is also what the grid needs
    /// before it can give the client a slot.
    pub(crate) fn wants_tx(&self) -> bool {
        self.tx.sample_rate_hz.is_some()
    }

    pub(crate) fn wants_rx(&self) -> bool {
        self.rx_configured
    }
}

/// Every client attached to one composite daemon.
pub(crate) struct ClientRegistry {
    config: CompositeConfig,
    clients: HashMap<SocketAddr, ClientSettings>,
}

impl ClientRegistry {
    pub(crate) fn new(config: CompositeConfig) -> Self {
        Self {
            config,
            clients: HashMap::new(),
        }
    }

    pub(crate) fn config(&self) -> &CompositeConfig {
        &self.config
    }

    pub(crate) fn len(&self) -> usize {
        self.clients.len()
    }

    pub(crate) fn get(&self, key: SocketAddr) -> Option<&ClientSettings> {
        self.clients.get(&key)
    }

    /// Drops a client and reports the session it held, so its slots can be released.
    /// The outer option says whether the client was known at all.
    pub(crate) fn forget(&mut self, key: SocketAddr) -> Option<Option<u32>> {
        self.clients.remove(&key).map(|client| client.session_id)
    }

    fn entry(&mut self, key: SocketAddr) -> &mut ClientSettings {
        self.clients.entry(key).or_default()
    }

    /// Records a TX rate and returns it, since a composite client keeps its own rate. A
    /// fractional rate comes back as the exact device fraction it matched.
    pub(crate) fn set_tx_sample_rate(
        &mut self,
        key: SocketAddr,
        sample_rate_hz: f64,
    ) -> Result<f64, Error> {
        let config = self.config;
        let sample_rate_hz = config.exact_client_rate_hz(sample_rate_hz)?;
        let client = self.entry(key);
        let mut candidate = client.tx;
        candidate.sample_rate_hz = Some(sample_rate_hz);
        candidate.check(&config, Direction::Tx)?;
        client.tx = candidate;
        Ok(sample_rate_hz)
    }

    pub(crate) fn set_tx_carrier(&mut self, key: SocketAddr, carrier_hz: u64) -> Result<(), Error> {
        let config = self.config;
        let client = self.entry(key);
        let mut candidate = client.tx;
        candidate.carrier_hz = Some(carrier_hz);
        candidate.check(&config, Direction::Tx)?;
        client.tx = candidate;
        Ok(())
    }

    pub(crate) fn set_tx_bandwidth(&mut self, key: SocketAddr, bandwidth_hz: u64) {
        self.entry(key).tx.bandwidth_hz = Some(bandwidth_hz);
    }

    /// Records an RX stream and returns the rate the client asked for.
    pub(crate) fn setup_rx(
        &mut self,
        key: SocketAddr,
        sample_rate_hz: f64,
        bandwidth_hz: Option<u64>,
    ) -> Result<f64, Error> {
        let config = self.config;
        let sample_rate_hz = config.exact_client_rate_hz(sample_rate_hz)?;
        let client = self.entry(key);
        let mut candidate = client.rx;
        candidate.sample_rate_hz = Some(sample_rate_hz);
        if let Some(bandwidth_hz) = bandwidth_hz {
            candidate.bandwidth_hz = Some(bandwidth_hz);
        }
        candidate.check(&config, Direction::Rx)?;
        client.rx = candidate;
        client.rx_configured = true;
        Ok(sample_rate_hz)
    }

    pub(crate) fn set_rx_carrier(&mut self, key: SocketAddr, carrier_hz: u64) -> Result<(), Error> {
        let config = self.config;
        let client = self.entry(key);
        let mut candidate = client.rx;
        candidate.carrier_hz = Some(carrier_hz);
        candidate.check(&config, Direction::Rx)?;
        client.rx = candidate;
        Ok(())
    }

    /// Plans both directions for a client opening its session.
    ///
    /// A client must have configured at least one direction. The error names the missing
    /// setting rather than the session, since that is what the client has to fix.
    pub(crate) fn plan(
        &self,
        key: SocketAddr,
    ) -> Result<(Option<SlicePlan>, Option<SlicePlan>), Error> {
        let client = self.clients.get(&key).ok_or_else(|| -> Error {
            "a composite session needs a configured TX or RX stream before it opens".into()
        })?;
        if !client.wants_tx() && !client.wants_rx() {
            return Err(
                "a composite session needs a configured TX or RX stream before it opens".into(),
            );
        }
        let tx = client
            .wants_tx()
            .then(|| client.tx.plan(&self.config, Direction::Tx))
            .transpose()?;
        let rx = client
            .wants_rx()
            .then(|| client.rx.plan(&self.config, Direction::Rx))
            .transpose()?;
        Ok((tx, rx))
    }

    /// Records the token and slots issued to a client's session.
    ///
    /// A slot is reused once the client holding it goes away, so whoever held it before
    /// is dropped here. That keeps the registry bounded by the number of slots rather
    /// than by how many control connections have ever arrived.
    pub(crate) fn open_session(
        &mut self,
        key: SocketAddr,
        session_id: u32,
        tx_slot: Option<usize>,
        rx_slot: Option<usize>,
    ) -> usize {
        let mut evicted = 0;
        self.clients.retain(|&held, client| {
            if held == key {
                return true;
            }
            let takes_tx = tx_slot.is_some() && client.tx_slot == tx_slot;
            let takes_rx = rx_slot.is_some() && client.rx_slot == rx_slot;
            if takes_tx || takes_rx {
                evicted += 1;
                return false;
            }
            true
        });
        let client = self.entry(key);
        client.session_id = Some(session_id);
        client.tx_slot = tx_slot;
        client.rx_slot = rx_slot;
        evicted
    }

    /// Records the fan-out slot a client holds, or that it holds none.
    pub(crate) fn set_rx_slot(&mut self, key: SocketAddr, slot: Option<usize>) {
        self.entry(key).rx_slot = slot;
    }

    /// Marks whether a client's receiver should run and returns how many now want it.
    pub(crate) fn set_rx_active(&mut self, key: SocketAddr, active: bool) -> usize {
        self.entry(key).rx_active = active;
        self.clients.values().filter(|c| c.rx_active).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: CompositeConfig = CompositeConfig {
        device_rate_hz: 3_888_000,
        tx_center_hz: 867_520_000,
        rx_center_hz: 822_520_000,
        tx_digital_backoff_db: -12.0,
        block_duration_ms: 5.0 / 6.0,
        tx_deadline_ms: 2.0,
    };
    const LOW: &str = "127.0.0.1:40001";
    const HIGH: &str = "127.0.0.1:40002";

    fn key(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    fn new_registry() -> ClientRegistry {
        ClientRegistry::new(CONFIG)
    }

    #[test]
    fn a_client_keeps_the_rate_it_asked_for() {
        let mut registry = new_registry();
        assert_eq!(
            registry.set_tx_sample_rate(key(LOW), 388_800.0).unwrap(),
            388_800.0
        );
        assert_eq!(
            registry.setup_rx(key(LOW), 388_800.0, None).unwrap(),
            388_800.0
        );
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn rational_rates_are_recorded_without_changing_the_requested_rate() {
        let mut registry = new_registry();
        assert_eq!(
            registry.set_tx_sample_rate(key(LOW), 100_000.0).unwrap(),
            100_000.0
        );
        assert_eq!(
            registry.get(key(LOW)).unwrap().tx.sample_rate_hz,
            Some(100_000.0)
        );
    }

    #[test]
    fn a_carrier_is_checked_once_the_rate_is_known_whichever_order_they_arrive() {
        // Carrier first, then a rate that makes it fit.
        let mut registry = new_registry();
        registry.set_tx_carrier(key(LOW), 869_040_000).unwrap();
        registry.set_tx_sample_rate(key(LOW), 388_800.0).unwrap();

        // Carrier first, then a rate wide enough to push it past the edge.
        let mut registry = new_registry();
        registry.set_tx_carrier(key(LOW), 869_040_000).unwrap();
        assert!(registry.set_tx_sample_rate(key(LOW), 3_888_000.0).is_err());

        // Rate first, then a carrier outside the span.
        let mut registry = new_registry();
        registry.set_tx_sample_rate(key(LOW), 388_800.0).unwrap();
        assert!(registry.set_tx_carrier(key(LOW), 880_000_000).is_err());
    }

    #[test]
    fn two_clients_keep_separate_settings() {
        let mut registry = new_registry();
        registry.set_tx_sample_rate(key(LOW), 97_200.0).unwrap();
        registry.set_tx_carrier(key(LOW), 866_000_000).unwrap();
        registry.set_tx_sample_rate(key(HIGH), 388_800.0).unwrap();
        registry.set_tx_carrier(key(HIGH), 869_040_000).unwrap();

        assert_eq!(registry.len(), 2);
        let (low_tx, low_rx) = registry.plan(key(LOW)).unwrap();
        let (high_tx, _) = registry.plan(key(HIGH)).unwrap();
        assert_eq!(low_tx.as_ref().unwrap().ratio, 40);
        assert_eq!(low_tx.as_ref().unwrap().offset_hz, -1_520_000);
        assert!(low_rx.is_none());
        assert_eq!(high_tx.as_ref().unwrap().ratio, 10);
        assert_eq!(high_tx.as_ref().unwrap().offset_hz, 1_520_000);
    }

    #[test]
    fn a_session_needs_at_least_one_configured_direction() {
        let registry = new_registry();
        let message = registry
            .plan(key(LOW))
            .expect_err("an unconfigured client cannot open")
            .to_string();
        assert!(message.contains("configured TX or RX stream"), "{message}");

        // Bandwidth alone is not a configured stream.
        let mut registry = new_registry();
        registry.set_tx_bandwidth(key(LOW), 200_000);
        assert!(registry.plan(key(LOW)).is_err());
    }

    #[test]
    fn a_stream_with_a_rate_but_no_carrier_opens_at_the_center() {
        // The standard client tunes after it splits, so this order has to work.
        let mut registry = new_registry();
        registry.set_tx_sample_rate(key(LOW), 388_800.0).unwrap();
        let (tx, rx) = registry.plan(key(LOW)).unwrap();
        assert_eq!(tx.expect("a TX slice at the center").offset_hz, 0);
        assert!(rx.is_none());

        // The retune then moves it.
        registry.set_tx_carrier(key(LOW), 866_000_000).unwrap();
        let (tx, _) = registry.plan(key(LOW)).unwrap();
        assert_eq!(tx.unwrap().offset_hz, -1_520_000);
    }

    #[test]
    fn an_rx_only_client_plans_rx_alone() {
        let mut registry = new_registry();
        registry
            .setup_rx(key(LOW), 388_800.0, Some(30_000))
            .unwrap();
        registry.set_rx_carrier(key(LOW), 821_000_000).unwrap();
        let (tx, rx) = registry.plan(key(LOW)).unwrap();
        assert!(tx.is_none());
        let rx = rx.expect("an RX-only client gets an RX slice");
        assert_eq!(rx.ratio, 10);
        assert_eq!(rx.offset_hz, -1_520_000);
    }

    #[test]
    fn a_bandwidth_from_setup_rx_sizes_the_slice_filter() {
        let mut registry = new_registry();
        registry
            .setup_rx(key(LOW), 388_800.0, Some(30_000))
            .unwrap();
        registry.set_rx_carrier(key(LOW), 821_000_000).unwrap();
        let narrow = registry.plan(key(LOW)).unwrap().1.unwrap();

        let mut registry = new_registry();
        registry.setup_rx(key(HIGH), 388_800.0, None).unwrap();
        registry.set_rx_carrier(key(HIGH), 821_000_000).unwrap();
        let wide = registry.plan(key(HIGH)).unwrap().1.unwrap();
        assert!(wide.taps.len() > narrow.taps.len());
    }

    #[test]
    fn opening_a_session_records_its_token_and_slots() {
        let mut registry = new_registry();
        registry.set_tx_sample_rate(key(LOW), 388_800.0).unwrap();
        registry.set_tx_sample_rate(key(HIGH), 388_800.0).unwrap();
        assert!(registry.get(key(LOW)).unwrap().session_id.is_none());

        registry.open_session(key(LOW), 0xabcd, Some(0), None);
        registry.open_session(key(HIGH), 0x1234, Some(1), Some(0));
        let low = registry.get(key(LOW)).unwrap();
        assert_eq!(
            (low.session_id, low.tx_slot, low.rx_slot),
            (Some(0xabcd), Some(0), None)
        );
        let high = registry.get(key(HIGH)).unwrap();
        assert_eq!(
            (high.session_id, high.tx_slot, high.rx_slot),
            (Some(0x1234), Some(1), Some(0))
        );
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn a_client_taking_over_a_slot_drops_whoever_held_it() {
        let mut registry = new_registry();
        registry.set_tx_sample_rate(key(LOW), 388_800.0).unwrap();
        registry.set_tx_sample_rate(key(HIGH), 388_800.0).unwrap();
        assert_eq!(registry.open_session(key(LOW), 0xaaaa, Some(0), None), 0);
        assert_eq!(registry.open_session(key(HIGH), 0xbbbb, Some(1), None), 0);
        assert_eq!(registry.len(), 2);

        // The low client died and its slot was released, so a new connection takes it.
        let rejoined: SocketAddr = "127.0.0.1:40003".parse().unwrap();
        registry.set_tx_sample_rate(rejoined, 388_800.0).unwrap();
        assert_eq!(registry.open_session(rejoined, 0xcccc, Some(0), None), 1);
        assert_eq!(registry.len(), 2, "the registry grew past the slot count");
        assert!(registry.get(key(LOW)).is_none());
        assert_eq!(registry.get(key(HIGH)).unwrap().session_id, Some(0xbbbb));
        assert_eq!(registry.get(rejoined).unwrap().tx_slot, Some(0));
    }

    #[test]
    fn reopening_a_session_from_the_same_connection_keeps_the_client() {
        let mut registry = new_registry();
        registry.set_tx_sample_rate(key(LOW), 388_800.0).unwrap();
        assert_eq!(registry.open_session(key(LOW), 0xaaaa, Some(0), Some(0)), 0);
        assert_eq!(registry.open_session(key(LOW), 0xdddd, Some(0), Some(0)), 0);
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.get(key(LOW)).unwrap().session_id, Some(0xdddd));
    }

    #[test]
    fn forgetting_a_client_reports_the_session_it_held() {
        let mut registry = new_registry();
        assert!(registry.forget(key(LOW)).is_none(), "an unknown client");

        registry.set_tx_sample_rate(key(LOW), 388_800.0).unwrap();
        // Known but never opened a session, so there is nothing to release.
        assert_eq!(registry.forget(key(LOW)), Some(None));

        registry.set_tx_sample_rate(key(HIGH), 388_800.0).unwrap();
        registry.open_session(key(HIGH), 0xbeef, Some(0), Some(0));
        assert_eq!(registry.forget(key(HIGH)), Some(Some(0xbeef)));
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn the_device_receiver_follows_how_many_clients_want_it() {
        let mut registry = new_registry();
        registry.setup_rx(key(LOW), 388_800.0, None).unwrap();
        registry.setup_rx(key(HIGH), 388_800.0, None).unwrap();
        assert_eq!(registry.set_rx_active(key(LOW), true), 1);
        assert_eq!(registry.set_rx_active(key(HIGH), true), 2);
        // One client stopping leaves the other wanting the receiver.
        assert_eq!(registry.set_rx_active(key(LOW), false), 1);
        assert_eq!(registry.set_rx_active(key(HIGH), false), 0);
    }
}
