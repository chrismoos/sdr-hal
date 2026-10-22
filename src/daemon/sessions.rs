//! Live sessions and the composite slots they feed.
//!
//! A data-plane packet names its session, so this table is what turns that token into the
//! stream worker's slot. Serving one client at a time keeps a single entry, which a
//! reconnecting client replaces.

use std::sync::Arc;

use parking_lot::RwLock;

use super::bridge::SharedReturnPath;

/// One client's session.
#[derive(Clone)]
pub(crate) struct Session {
    /// Server-issued token every packet from this client must name.
    pub(crate) session_id: u32,
    /// Where this client's RX IQ and clock heartbeats are sent.
    pub(crate) peer: SharedReturnPath,
    /// TX grid slot, absent for a receive-only client.
    pub(crate) tx_slot: Option<usize>,
    /// RX fan-out slot, absent for a transmit-only client.
    pub(crate) rx_slot: Option<usize>,
}

/// Every open session, shared between the control service and both stream workers.
pub(crate) struct Sessions {
    entries: RwLock<Vec<Session>>,
    /// Bumped whenever the set changes, so a worker can notice without holding the lock.
    generation: std::sync::atomic::AtomicU64,
    /// Slots of sessions that ended, waiting for their worker to release them. A slot is
    /// named here rather than worked out by comparing the table, because a slot exists
    /// before its session is inserted and a comparison would reclaim it in that window.
    released_tx: parking_lot::Mutex<Vec<usize>>,
    released_rx: parking_lot::Mutex<Vec<usize>>,
}

impl Sessions {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            entries: RwLock::new(Vec::new()),
            generation: std::sync::atomic::AtomicU64::new(0),
            released_tx: parking_lot::Mutex::new(Vec::new()),
            released_rx: parking_lot::Mutex::new(Vec::new()),
        })
    }

    /// Replaces every session with one, which is how a single-client daemon supersedes a
    /// reconnecting client.
    pub(crate) fn replace_with_one(&self, session: Session) {
        let mut entries = self.entries.write();
        entries.clear();
        entries.push(session);
        self.bump();
    }

    /// Adds a session, replacing any entry already holding its slots or token.
    pub(crate) fn insert(&self, session: Session) {
        let mut entries = self.entries.write();
        entries.retain(|open| {
            open.session_id != session.session_id
                && (session.tx_slot.is_none() || open.tx_slot != session.tx_slot)
                && (session.rx_slot.is_none() || open.rx_slot != session.rx_slot)
        });
        entries.push(session);
        self.bump();
    }

    /// Drops one session and queues the composite slots it held for release.
    pub(crate) fn remove(&self, session_id: u32) {
        let mut entries = self.entries.write();
        let Some(position) = entries
            .iter()
            .position(|open| open.session_id == session_id)
        else {
            return;
        };
        let gone = entries.remove(position);
        if let Some(slot) = gone.tx_slot {
            self.released_tx.lock().push(slot);
        }
        if let Some(slot) = gone.rx_slot {
            self.released_rx.lock().push(slot);
        }
        self.bump();
    }

    /// Takes the TX slots whose sessions ended since the last call.
    pub(crate) fn take_released_tx(&self) -> Vec<usize> {
        std::mem::take(&mut *self.released_tx.lock())
    }

    /// Takes the RX slots whose sessions ended since the last call.
    pub(crate) fn take_released_rx(&self) -> Vec<usize> {
        std::mem::take(&mut *self.released_rx.lock())
    }

    pub(crate) fn clear(&self) {
        let mut entries = self.entries.write();
        if !entries.is_empty() {
            entries.clear();
            self.bump();
        }
    }

    fn bump(&self) {
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    /// Changes to the set since the last read, for a worker that resets per-session state.
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.read().len()
    }

    /// Looks up a session by the token a packet names.
    pub(crate) fn find(&self, session_id: u32) -> Option<Session> {
        self.entries
            .read()
            .iter()
            .find(|open| open.session_id == session_id)
            .cloned()
    }

    /// Whether any open session names this token, which is the data plane's filter.
    pub(crate) fn accepts(&self, session_id: u32) -> bool {
        self.entries
            .read()
            .iter()
            .any(|open| open.session_id == session_id)
    }

    pub(crate) fn all(&self) -> Vec<Session> {
        self.entries.read().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    fn session(session_id: u32, tx_slot: Option<usize>, rx_slot: Option<usize>) -> Session {
        Session {
            session_id,
            peer: Arc::new(Mutex::new(None)),
            tx_slot,
            rx_slot,
        }
    }

    #[test]
    fn an_empty_table_accepts_nothing() {
        let sessions = Sessions::new();
        assert_eq!(sessions.len(), 0);
        assert!(!sessions.accepts(1));
        assert!(sessions.find(1).is_none());
        assert!(sessions.all().is_empty());
    }

    #[test]
    fn replacing_with_one_supersedes_whatever_was_open() {
        let sessions = Sessions::new();
        sessions.replace_with_one(session(0xaaaa, Some(0), Some(0)));
        assert!(sessions.accepts(0xaaaa));
        sessions.replace_with_one(session(0xbbbb, Some(0), Some(0)));
        assert_eq!(sessions.len(), 1);
        assert!(!sessions.accepts(0xaaaa));
        assert_eq!(sessions.all()[0].session_id, 0xbbbb);
    }

    #[test]
    fn several_sessions_coexist_and_each_is_found_by_its_token() {
        let sessions = Sessions::new();
        sessions.insert(session(0xaaaa, Some(0), Some(0)));
        sessions.insert(session(0xbbbb, Some(1), Some(1)));
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions.find(0xaaaa).unwrap().tx_slot, Some(0));
        assert_eq!(sessions.find(0xbbbb).unwrap().rx_slot, Some(1));
        assert!(!sessions.accepts(0xcccc));
    }

    #[test]
    fn a_session_reclaiming_a_slot_evicts_the_one_holding_it() {
        let sessions = Sessions::new();
        sessions.insert(session(0xaaaa, Some(0), None));
        sessions.insert(session(0xbbbb, Some(1), None));
        // The same client reconnecting takes its slot back under a new token.
        sessions.insert(session(0xcccc, Some(0), None));
        assert_eq!(sessions.len(), 2);
        assert!(!sessions.accepts(0xaaaa));
        assert!(sessions.accepts(0xbbbb));
        assert!(sessions.accepts(0xcccc));
    }

    #[test]
    fn receive_only_sessions_do_not_collide_on_an_absent_tx_slot() {
        let sessions = Sessions::new();
        sessions.insert(session(0xaaaa, None, Some(0)));
        sessions.insert(session(0xbbbb, None, Some(1)));
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn a_repeated_token_replaces_rather_than_duplicating() {
        let sessions = Sessions::new();
        sessions.insert(session(0xaaaa, Some(0), None));
        sessions.insert(session(0xaaaa, Some(3), None));
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions.find(0xaaaa).unwrap().tx_slot, Some(3));
    }

    #[test]
    fn clearing_takes_every_session_out_of_the_table() {
        let sessions = Sessions::new();
        sessions.insert(session(0xaaaa, Some(0), None));
        sessions.insert(session(0xbbbb, Some(1), None));
        sessions.clear();
        assert_eq!(sessions.len(), 0);
        assert!(!sessions.accepts(0xaaaa));
        assert!(!sessions.accepts(0xbbbb));
    }

    #[test]
    fn a_removed_session_queues_its_slots_for_release() {
        let sessions = Sessions::new();
        sessions.insert(session(0xaaaa, Some(0), Some(3)));
        sessions.insert(session(0xbbbb, Some(1), Some(1)));
        let before = sessions.generation();

        sessions.remove(0xaaaa);
        assert_eq!(sessions.len(), 1);
        assert!(!sessions.accepts(0xaaaa));
        assert!(sessions.generation() > before);
        // Each worker takes only its own direction's slots, and only once.
        assert_eq!(sessions.take_released_tx(), vec![0]);
        assert_eq!(sessions.take_released_rx(), vec![3]);
        assert!(sessions.take_released_tx().is_empty());
        assert!(sessions.take_released_rx().is_empty());

        // Removing what is already gone changes nothing, so a worker is not woken for it.
        let after = sessions.generation();
        sessions.remove(0xaaaa);
        assert_eq!(sessions.generation(), after);
        assert!(sessions.take_released_tx().is_empty());
        // The surviving session keeps its slots.
        assert_eq!(sessions.find(0xbbbb).unwrap().tx_slot, Some(1));
    }

    #[test]
    fn a_receive_only_session_queues_no_transmit_slot() {
        let sessions = Sessions::new();
        sessions.insert(session(0xaaaa, None, Some(2)));
        sessions.remove(0xaaaa);
        assert!(sessions.take_released_tx().is_empty());
        assert_eq!(sessions.take_released_rx(), vec![2]);
    }

    #[test]
    fn the_generation_moves_only_when_the_set_changes() {
        let sessions = Sessions::new();
        let start = sessions.generation();
        sessions.insert(session(0xaaaa, Some(0), None));
        let after_insert = sessions.generation();
        assert!(after_insert > start);

        sessions.clear();
        assert!(sessions.generation() > after_insert);
        let cleared = sessions.generation();
        sessions.clear();
        assert_eq!(sessions.generation(), cleared);
    }

    #[test]
    fn all_returns_every_open_session_for_a_fan_out() {
        let sessions = Sessions::new();
        sessions.insert(session(0xaaaa, None, Some(0)));
        sessions.insert(session(0xbbbb, None, Some(1)));
        let slots: Vec<_> = sessions.all().iter().map(|open| open.rx_slot).collect();
        assert_eq!(slots, vec![Some(0), Some(1)]);
    }
}
