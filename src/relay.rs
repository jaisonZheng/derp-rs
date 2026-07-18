use std::{
    collections::HashSet,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};

use bytes::Bytes;
use dashmap::{DashMap, DashSet, mapref::entry::Entry};
use parking_lot::Mutex;
use smallvec::SmallVec;
use subtle::ConstantTimeEq;
use tokio::sync::mpsc;

use crate::{
    crypto::NodeKey,
    metrics::Metrics,
    protocol::{Frame, peer_gone_reason, peer_present_flags},
};

pub type SessionId = u64;

pub struct SessionHandle {
    pub id: SessionId,
    pub key: NodeKey,
    pub endpoint: SocketAddr,
    pub can_mesh: bool,
    pub can_ack_pings: bool,
    pub is_prober: bool,
    pub not_ideal: bool,
    tx: mpsc::Sender<Frame>,
    closed: AtomicBool,
    duplicate: AtomicBool,
    preferred: AtomicBool,
    preferred_seen: AtomicBool,
    seen_sources: Mutex<HashSet<NodeKey>>,
    peer_gone_limiter: Mutex<PeerGoneLimiter>,
}

impl SessionHandle {
    fn flags(&self) -> u8 {
        let mut f = 0;
        if self.can_mesh {
            f |= peer_present_flags::MESH;
        }
        if self.is_prober {
            f |= peer_present_flags::PROBER;
        }
        if self.not_ideal {
            f |= peer_present_flags::NOT_IDEAL;
        }
        if f == 0 {
            peer_present_flags::REGULAR
        } else {
            f
        }
    }
    pub fn send(&self, frame: Frame, metrics: &Metrics) -> bool {
        match self.tx.try_send(frame) {
            Ok(()) => true,
            Err(_) => {
                Metrics::inc(&metrics.queue_dropped, 1);
                false
            }
        }
    }
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
    fn send_peer_not_here(&self, peer: NodeKey, packet: &[u8], metrics: &Metrics) {
        if looks_like_disco_wrapper(packet) && self.peer_gone_limiter.lock().allow() {
            self.send(
                Frame::PeerGone {
                    peer,
                    reason: peer_gone_reason::NOT_HERE,
                },
                metrics,
            );
        }
    }
}

struct PeerGoneLimiter {
    tokens: f64,
    updated: Instant,
}

impl PeerGoneLimiter {
    fn new() -> Self {
        Self {
            tokens: 3.0,
            updated: Instant::now(),
        }
    }

    fn allow(&mut self) -> bool {
        let now = Instant::now();
        self.tokens = (self.tokens + now.duration_since(self.updated).as_secs_f64() * 3.0).min(3.0);
        self.updated = now;
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }
}

fn looks_like_disco_wrapper(packet: &[u8]) -> bool {
    const DISCO_MAGIC: &[u8; 6] = b"TS\xF0\x9F\x92\xAC";
    packet.len() >= DISCO_MAGIC.len() + 32 + 24 && packet.starts_with(DISCO_MAGIC)
}

struct PeerSet {
    sessions: SmallVec<[Arc<SessionHandle>; 1]>,
    active: SessionId,
}
impl PeerSet {
    fn active(&self) -> Option<Arc<SessionHandle>> {
        self.sessions
            .iter()
            .find(|s| s.id == self.active)
            .or_else(|| self.sessions.last())
            .cloned()
    }
}

pub struct RegisteredSession {
    pub handle: Arc<SessionHandle>,
    pub rx: mpsc::Receiver<Frame>,
}

pub struct Relay {
    peers: DashMap<NodeKey, Arc<Mutex<PeerSet>>>,
    // Separate hot-path index. Session-set bookkeeping needs a mutex for
    // duplicate keys, while packet delivery only needs the current sender.
    active: DashMap<NodeKey, Arc<SessionHandle>>,
    reverse_paths: DashMap<NodeKey, DashSet<NodeKey>>,
    watchers: DashMap<SessionId, Arc<SessionHandle>>,
    mesh_routes: DashMap<NodeKey, Vec<MeshRoute>>,
    next_id: AtomicU64,
    mesh_key: Option<[u8; 32]>,
    queue_depth: usize,
    pub metrics: Arc<Metrics>,
}

#[derive(Clone)]
struct MeshRoute {
    mesh_id: u64,
    tx: mpsc::Sender<Frame>,
}

impl Relay {
    pub fn new(queue_depth: usize, mesh_key: Option<[u8; 32]>) -> Arc<Self> {
        Arc::new(Self {
            peers: DashMap::new(),
            active: DashMap::new(),
            reverse_paths: DashMap::new(),
            watchers: DashMap::new(),
            mesh_routes: DashMap::new(),
            next_id: AtomicU64::new(1),
            mesh_key,
            queue_depth: queue_depth.max(1),
            metrics: Arc::new(Metrics::default()),
        })
    }
    pub fn client_count(&self) -> usize {
        self.peers
            .iter()
            .map(|p| p.value().lock().sessions.len())
            .sum()
    }
    pub fn public_peer_count(&self) -> usize {
        self.peers.len()
    }
    pub fn is_mesh_key(&self, value: Option<[u8; 32]>) -> bool {
        match (self.mesh_key, value) {
            (Some(a), Some(b)) => bool::from(a.ct_eq(&b)),
            _ => false,
        }
    }

    pub fn register(
        &self,
        key: NodeKey,
        endpoint: SocketAddr,
        can_mesh: bool,
        can_ack_pings: bool,
        is_prober: bool,
        not_ideal: bool,
    ) -> RegisteredSession {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(self.queue_depth);
        let handle = Arc::new(SessionHandle {
            id,
            key,
            endpoint,
            can_mesh,
            can_ack_pings,
            is_prober,
            not_ideal,
            tx,
            closed: AtomicBool::new(false),
            duplicate: AtomicBool::new(false),
            preferred: AtomicBool::new(false),
            preferred_seen: AtomicBool::new(false),
            seen_sources: Mutex::new(HashSet::new()),
            peer_gone_limiter: Mutex::new(PeerGoneLimiter::new()),
        });
        let set = match self.peers.entry(key) {
            Entry::Occupied(e) => e.get().clone(),
            Entry::Vacant(e) => e
                .insert(Arc::new(Mutex::new(PeerSet {
                    sessions: SmallVec::new(),
                    active: id,
                })))
                .clone(),
        };
        {
            let mut set = set.lock();
            set.sessions.push(Arc::clone(&handle));
            set.active = id;
            self.active.insert(key, Arc::clone(&handle));
            if set.sessions.len() > 1 {
                for session in &set.sessions {
                    session.duplicate.store(true, Ordering::Release);
                    session.send(
                        Frame::Health(Bytes::from_static(
                            b"duplicate DERP connection for node key",
                        )),
                        &self.metrics,
                    );
                }
            }
        }
        Metrics::inc(&self.metrics.current_clients, 1);
        self.broadcast_presence(&handle, true);
        RegisteredSession { handle, rx }
    }

    pub fn note_activity(&self, handle: &SessionHandle) {
        if !handle.duplicate.load(Ordering::Acquire) {
            return;
        }
        if let Some(set) = self.peers.get(&handle.key) {
            let mut set = set.lock();
            set.active = handle.id;
            if let Some(session) = set.active() {
                self.active.insert(handle.key, session);
            }
        }
    }

    pub fn note_preferred(&self, handle: &SessionHandle, preferred: bool) {
        let was_preferred = handle.preferred.swap(preferred, Ordering::AcqRel);
        let was_seen = handle.preferred_seen.swap(true, Ordering::AcqRel);
        if was_preferred == preferred {
            return;
        }
        if preferred {
            Metrics::inc(&self.metrics.preferred_clients, 1);
            if was_seen {
                Metrics::inc(&self.metrics.home_moves_in, 1);
            }
        } else {
            Metrics::dec(&self.metrics.preferred_clients);
            if was_seen {
                Metrics::inc(&self.metrics.home_moves_out, 1);
            }
        }
    }

    pub fn unregister(&self, handle: &SessionHandle) {
        self.watchers.remove(&handle.id);
        handle.close();
        if handle.preferred.swap(false, Ordering::AcqRel) {
            Metrics::dec(&self.metrics.preferred_clients);
        }
        let mut gone = false;
        if let Some(set_ref) = self.peers.get(&handle.key) {
            let set_arc = set_ref.clone();
            drop(set_ref);
            let mut set = set_arc.lock();
            set.sessions.retain(|s| s.id != handle.id);
            if set.sessions.is_empty() {
                gone = true;
            } else {
                set.active = set.sessions.last().unwrap().id;
                self.active
                    .insert(handle.key, set.sessions.last().unwrap().clone());
                if set.sessions.len() == 1 {
                    set.sessions[0].duplicate.store(false, Ordering::Release);
                    set.sessions[0].send(Frame::Health(Bytes::new()), &self.metrics);
                }
            }
        }
        if gone {
            self.peers.remove(&handle.key);
            self.active.remove(&handle.key);
            self.broadcast_presence(handle, false);
            if let Some((_, watchers)) = self.reverse_paths.remove(&handle.key) {
                for watcher in watchers.iter() {
                    if let Some(dst) = self.active(*watcher) {
                        dst.send(
                            Frame::PeerGone {
                                peer: handle.key,
                                reason: peer_gone_reason::DISCONNECTED,
                            },
                            &self.metrics,
                        );
                    }
                }
            }
            for paths in self.reverse_paths.iter() {
                paths.value().remove(&handle.key);
            }
        }
        Metrics::dec(&self.metrics.current_clients);
    }

    fn active(&self, key: NodeKey) -> Option<Arc<SessionHandle>> {
        self.active
            .get(&key)
            .map(|session| Arc::clone(session.value()))
    }

    pub fn route(
        &self,
        source: &SessionHandle,
        src: NodeKey,
        dst: NodeKey,
        packet: Bytes,
        forwarded: bool,
    ) {
        Metrics::inc(&self.metrics.packets_received, 1);
        Metrics::inc(&self.metrics.bytes_received, packet.len() as u64);
        if let Some(dest) = self.active(dst) {
            let packet_len = packet.len();
            if dest.send(Frame::RecvPacket { src, packet }, &self.metrics) {
                Metrics::inc(&self.metrics.packets_sent, 1);
                Metrics::inc(&self.metrics.bytes_sent, packet_len as u64);
                if dest.seen_sources.lock().insert(src) {
                    self.reverse_paths.entry(src).or_default().insert(dst);
                }
            } else {
                Metrics::inc(&self.metrics.packets_dropped, 1);
            }
        } else if !forwarded && self.forward_to_mesh(src, dst, packet.clone()) {
            Metrics::inc(&self.metrics.mesh_forwarded_out, 1);
        } else {
            Metrics::inc(&self.metrics.packets_dropped, 1);
            source.send_peer_not_here(dst, &packet, &self.metrics);
        }
        if forwarded {
            Metrics::inc(&self.metrics.mesh_forwarded_in, 1);
        }
    }

    fn forward_to_mesh(&self, src: NodeKey, dst: NodeKey, packet: Bytes) -> bool {
        let Some(routes) = self.mesh_routes.get(&dst) else {
            return false;
        };
        for route in routes.iter() {
            if route
                .tx
                .try_send(Frame::ForwardPacket {
                    src,
                    dst,
                    packet: packet.clone(),
                })
                .is_ok()
            {
                return true;
            }
        }
        false
    }

    pub fn add_mesh_route(&self, mesh_id: u64, peer: NodeKey, tx: mpsc::Sender<Frame>) {
        let mut routes = self.mesh_routes.entry(peer).or_default();
        if !routes.iter().any(|route| route.mesh_id == mesh_id) {
            routes.push(MeshRoute { mesh_id, tx });
        }
    }

    pub fn remove_mesh_route(&self, mesh_id: u64, peer: NodeKey) {
        if let Some(mut routes) = self.mesh_routes.get_mut(&peer) {
            routes.retain(|route| route.mesh_id != mesh_id);
            let empty = routes.is_empty();
            drop(routes);
            if empty {
                self.mesh_routes.remove(&peer);
            }
        }
    }

    pub fn remove_mesh(&self, mesh_id: u64) {
        let peers: Vec<_> = self
            .mesh_routes
            .iter()
            .filter(|routes| routes.iter().any(|r| r.mesh_id == mesh_id))
            .map(|routes| *routes.key())
            .collect();
        for peer in peers {
            self.remove_mesh_route(mesh_id, peer);
        }
    }

    pub fn watch(&self, watcher: &Arc<SessionHandle>) -> bool {
        if !watcher.can_mesh {
            return false;
        }
        self.watchers.insert(watcher.id, Arc::clone(watcher));
        for peer in self.peers.iter() {
            let set = peer.value().lock();
            if let Some(active) = set.active() {
                watcher.send(
                    Frame::PeerPresent {
                        peer: *peer.key(),
                        endpoint: Some(active.endpoint),
                        flags: Some(active.flags()),
                        extra: Bytes::new(),
                    },
                    &self.metrics,
                );
            }
        }
        true
    }

    pub fn close_peer(&self, requester: &SessionHandle, peer: NodeKey) -> bool {
        if !requester.can_mesh {
            return false;
        }
        if let Some(set) = self.peers.get(&peer) {
            for s in &set.lock().sessions {
                s.close();
            }
            true
        } else {
            false
        }
    }

    fn broadcast_presence(&self, peer: &SessionHandle, present: bool) {
        let frame = if present {
            Frame::PeerPresent {
                peer: peer.key,
                endpoint: Some(peer.endpoint),
                flags: Some(peer.flags()),
                extra: Bytes::new(),
            }
        } else {
            Frame::PeerGone {
                peer: peer.key,
                reason: peer_gone_reason::DISCONNECTED,
            }
        };
        for watcher in self.watchers.iter() {
            if watcher.id != peer.id {
                watcher.send(frame.clone(), &self.metrics);
            }
        }
    }

    pub fn broadcast_restart(&self, reconnect_in_ms: u32, try_for_ms: u32) {
        for peer in self.peers.iter() {
            for session in &peer.value().lock().sessions {
                session.send(
                    Frame::Restarting {
                        reconnect_in_ms,
                        try_for_ms,
                    },
                    &self.metrics,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(n: u8) -> NodeKey {
        NodeKey::new([n; 32]).unwrap()
    }
    fn ep(n: u16) -> SocketAddr {
        format!("127.0.0.1:{n}").parse().unwrap()
    }
    #[tokio::test]
    async fn routes_and_reports_disconnect() {
        let relay = Relay::new(8, None);
        let a = relay.register(key(1), ep(1), false, true, false, false);
        let mut b = relay.register(key(2), ep(2), false, true, false, false);
        relay.route(
            &a.handle,
            key(1),
            key(2),
            Bytes::from_static(b"packet"),
            false,
        );
        assert_eq!(
            b.rx.recv().await,
            Some(Frame::RecvPacket {
                src: key(1),
                packet: Bytes::from_static(b"packet")
            })
        );
        relay.unregister(&a.handle);
        assert_eq!(
            b.rx.recv().await,
            Some(Frame::PeerGone {
                peer: key(1),
                reason: 0
            })
        );
    }
    #[tokio::test]
    async fn duplicate_health_is_cleared() {
        let relay = Relay::new(8, None);
        let mut a = relay.register(key(1), ep(1), false, true, false, false);
        let b = relay.register(key(1), ep(2), false, true, false, false);
        assert!(matches!(a.rx.recv().await, Some(Frame::Health(_))));
        relay.unregister(&b.handle);
        assert_eq!(a.rx.recv().await, Some(Frame::Health(Bytes::new())));
    }

    #[tokio::test]
    async fn not_here_is_only_sent_for_disco_and_is_rate_limited() {
        let relay = Relay::new(8, None);
        let mut a = relay.register(key(1), ep(1), false, true, false, false);
        relay.route(
            &a.handle,
            key(1),
            key(2),
            Bytes::from_static(b"ordinary packet"),
            false,
        );
        assert!(a.rx.try_recv().is_err());

        let mut disco = vec![0u8; 62];
        disco[..6].copy_from_slice(b"TS\xF0\x9F\x92\xAC");
        for _ in 0..4 {
            relay.route(
                &a.handle,
                key(1),
                key(2),
                Bytes::copy_from_slice(&disco),
                false,
            );
        }
        for _ in 0..3 {
            assert_eq!(
                a.rx.recv().await,
                Some(Frame::PeerGone {
                    peer: key(2),
                    reason: peer_gone_reason::NOT_HERE
                })
            );
        }
        assert!(a.rx.try_recv().is_err());
    }

    #[test]
    fn preferred_gauge_tracks_state_and_disconnect() {
        let relay = Relay::new(8, None);
        let a = relay.register(key(1), ep(1), false, true, false, false);
        relay.note_preferred(&a.handle, true);
        assert_eq!(
            relay
                .metrics
                .preferred_clients
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        relay.note_preferred(&a.handle, false);
        assert_eq!(
            relay
                .metrics
                .preferred_clients
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        relay.note_preferred(&a.handle, true);
        relay.unregister(&a.handle);
        assert_eq!(
            relay
                .metrics
                .preferred_clients
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }
}
