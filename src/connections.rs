use bytes::Bytes;
use hudsucker::{WebSocketContext, hyper::Uri};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::{mpsc, watch};

const INJECTION_QUEUE_CAPACITY: usize = 16;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ConnectionKey {
    client_addr: SocketAddr,
    server_uri: Uri,
}

impl ConnectionKey {
    pub fn new(client_addr: SocketAddr, server_uri: Uri) -> Self {
        Self {
            client_addr,
            server_uri,
        }
    }

    pub fn from_context(ctx: &WebSocketContext) -> Self {
        match ctx {
            WebSocketContext::ClientToServer { src, dst, .. } => Self::new(*src, dst.clone()),
            WebSocketContext::ServerToClient { src, dst, .. } => Self::new(*dst, src.clone()),
        }
    }

    pub fn is_observer(&self) -> bool {
        self.server_uri.path() == "/ob"
    }
}

#[derive(Default)]
pub(crate) struct Connections {
    states: Mutex<HashMap<ConnectionKey, ConnectionEntry>>,
}

struct ConnectionEntry {
    state: Weak<ConnectionState>,
    pending_peer: Option<Arc<ConnectionState>>,
    directions: u8,
}

impl Connections {
    pub fn get(self: &Arc<Self>, key: ConnectionKey, from_client: bool) -> Arc<ConnectionState> {
        let direction = if from_client { 1 } else { 2 };
        let mut states = self
            .states
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(entry) = states.get_mut(&key)
            && let Some(state) = entry.state.upgrade()
        {
            entry.directions |= direction;
            if entry.directions == 3 {
                let _ = entry.pending_peer.take();
            }
            return state;
        }

        let (injection_tx, injection_rx) = mpsc::channel(INJECTION_QUEUE_CAPACITY);
        let (closed, _) = watch::channel(false);
        let state = Arc::new(ConnectionState {
            key: key.clone(),
            connections: Arc::downgrade(self),
            pending_requests: Mutex::new(HashMap::new()),
            injection_tx,
            injection_rx: Mutex::new(Some(injection_rx)),
            closed,
        });
        // Both callbacks run synchronously, but the first forwarder can finish
        // on another worker before the second registers. Preserve its close signal.
        states.insert(
            key,
            ConnectionEntry {
                state: Arc::downgrade(&state),
                pending_peer: Some(Arc::clone(&state)),
                directions: direction,
            },
        );
        state
    }
}

pub(crate) struct ConnectionState {
    key: ConnectionKey,
    connections: Weak<Connections>,
    pending_requests: Mutex<HashMap<u16, String>>,
    pub injection_tx: mpsc::Sender<Bytes>,
    injection_rx: Mutex<Option<mpsc::Receiver<Bytes>>>,
    closed: watch::Sender<bool>,
}

impl ConnectionState {
    pub fn track_request(&self, id: u16, method_name: String) {
        self.pending_requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(id, method_name);
    }

    pub fn take_request(&self, id: u16) -> Option<String> {
        self.pending_requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&id)
    }

    pub fn take_injections(&self) -> Option<mpsc::Receiver<Bytes>> {
        self.injection_rx
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }

    pub fn subscribe_close(&self) -> watch::Receiver<bool> {
        self.closed.subscribe()
    }

    pub fn close(&self) {
        self.closed.send_replace(true);
    }
}

impl Drop for ConnectionState {
    fn drop(&mut self) {
        let Some(connections) = self.connections.upgrade() else {
            return;
        };
        let mut states = connections
            .states
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // A replacement connection may have reused the key before this drop acquired the lock.
        if states
            .get(&self.key)
            .is_some_and(|entry| std::ptr::eq(entry.state.as_ptr(), self))
        {
            states.remove(&self.key);
            if states.is_empty() {
                *states = HashMap::new();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(port: u16) -> ConnectionKey {
        ConnectionKey {
            client_addr: SocketAddr::from(([127, 0, 0, 1], port)),
            server_uri: "wss://game.example.test/gateway".parse().unwrap(),
        }
    }

    #[test]
    fn directions_share_requests_but_connections_do_not() {
        let connections = Arc::new(Connections::default());
        let outgoing = connections.get(key(10_001), true);
        let incoming = connections.get(key(10_001), false);
        let other = connections.get(key(10_002), true);
        let other_incoming = connections.get(key(10_002), false);
        assert!(Arc::ptr_eq(&outgoing, &incoming));

        outgoing.track_request(7, ".lq.Lobby.login".into());
        other.track_request(7, ".lq.FastTest.authGame".into());
        assert_eq!(incoming.take_request(7).as_deref(), Some(".lq.Lobby.login"));
        assert_eq!(
            other_incoming.take_request(7).as_deref(),
            Some(".lq.FastTest.authGame")
        );
        assert!(incoming.take_request(7).is_none());
    }

    #[test]
    fn last_direction_releases_pending_requests_and_registry_entry() {
        let connections = Arc::new(Connections::default());
        let first = connections.get(key(10_001), true);
        let second = connections.get(key(10_001), false);
        first.track_request(42, ".lq.Lobby.login".into());
        let weak = Arc::downgrade(&first);

        drop(first);
        assert!(weak.upgrade().is_some());
        drop(second);
        assert!(weak.upgrade().is_none());
        assert!(connections.states.lock().unwrap().is_empty());

        let reconnected = connections.get(key(10_001), true);
        let _peer = connections.get(key(10_001), false);
        assert!(reconnected.take_request(42).is_none());
    }

    #[test]
    fn injection_queue_has_one_receiver_and_applies_backpressure() {
        let connections = Arc::new(Connections::default());
        let state = connections.get(key(10_001), true);
        let _peer = connections.get(key(10_001), false);
        let mut receiver = state.take_injections().unwrap();
        assert!(state.take_injections().is_none());

        for _ in 0..INJECTION_QUEUE_CAPACITY {
            state
                .injection_tx
                .try_send(Bytes::from_static(b"message"))
                .unwrap();
        }
        assert!(matches!(
            state.injection_tx.try_send(Bytes::new()),
            Err(mpsc::error::TrySendError::Full(_))
        ));
        assert_eq!(receiver.try_recv().unwrap(), Bytes::from_static(b"message"));
        state.injection_tx.try_send(Bytes::new()).unwrap();
        drop(receiver);
        assert!(matches!(
            state.injection_tx.try_send(Bytes::new()),
            Err(mpsc::error::TrySendError::Closed(_))
        ));
    }

    #[test]
    fn early_disconnect_is_visible_when_the_other_direction_registers_later() {
        let connections = Arc::new(Connections::default());
        let first = connections.get(key(10_001), false);
        let weak = Arc::downgrade(&first);
        first.close();
        drop(first);

        let second = connections.get(key(10_001), true);
        assert!(Arc::ptr_eq(&weak.upgrade().unwrap(), &second));
        assert!(*second.subscribe_close().borrow());
        drop(second);
        assert!(weak.upgrade().is_none());
        assert!(connections.states.lock().unwrap().is_empty());
    }
}
