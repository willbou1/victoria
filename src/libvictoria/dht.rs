use anyhow::Result;
use bencodedit::{BencodeError, BencodeErrorKind};
use rand::{RngExt, rng};
use sha1::Digest;
use std::{
    array,
    collections::{HashMap, HashSet},
    net::{SocketAddr, SocketAddrV4, SocketAddrV6},
    time::{Duration, Instant},
};
use tokio::{
    select,
    sync::{mpsc, watch},
};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, info_span, warn};

use super::{
    proto::dht::*,
    torrent::{
        Event,
        control::{Config, DHTInfo, DHTNodeInfo},
    },
    types::*,
};

const GOOD_INTERVAL: Duration = Duration::from_mins(1);
#[derive(Debug)]
struct EndpointStatistics {
    last_sent: Instant,
    num_failed_queries: usize,
}

impl Default for EndpointStatistics {
    fn default() -> Self {
        Self {
            last_sent: Instant::now(),
            num_failed_queries: 0,
        }
    }
}

#[derive(Debug)]
struct Node<A> {
    id: NodeId,
    pub addrs: HashMap<A, EndpointStatistics>,
}

impl<A: std::hash::Hash + Eq + Copy> Node<A> {
    pub fn new(id: NodeId, addr: A) -> Self {
        Self {
            id,
            addrs: HashMap::from([(addr, EndpointStatistics::default())]),
        }
    }

    pub fn add_addr(&mut self, addr: A) {
        self.addrs.entry(addr).or_default();
    }

    pub fn refresh_addr(&mut self, addr: A) {
        self.addrs
            .entry(addr)
            .and_modify(|e| e.last_sent = Instant::now());
    }

    pub fn fail_addr(&mut self, addr: A) {
        self.addrs
            .entry(addr)
            .and_modify(|e| e.num_failed_queries += 1);
    }

    pub fn best_addr(&self) -> Option<A> {
        self.addrs
            .iter()
            .min_by(|a, b| {
                a.1.num_failed_queries
                    .cmp(&b.1.num_failed_queries)
                    .then(b.1.last_sent.cmp(&a.1.last_sent))
            })
            .map(|(addr, _)| *addr)
    }
}

#[derive(Debug)]
struct Bucket<A> {
    nodes: HashMap<NodeId, Node<A>>,
    last_changed: Instant,
}

impl<A: std::hash::Hash + Eq + Copy> Bucket<A> {
    const MAX_NUM_NODES_PER_BUCKET: usize = 8;

    pub fn is_full(&self) -> bool {
        self.nodes.len() >= Self::MAX_NUM_NODES_PER_BUCKET
    }

    pub fn is_maintainable(&self) -> bool {
        !self.is_full()
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn add_addr(&mut self, id: NodeId, addr: A) -> bool {
        let mut exists = false;
        if let Some(node) = self.nodes.get_mut(&id) {
            node.add_addr(addr);
            exists = true;
        }
        if exists {
            self.refresh_addr(&id, addr);
            return false;
        }
        if !self.is_full() {
            self.last_changed = Instant::now();
            self.nodes.insert(id, Node::new(id, addr));
            return true;
        }
        return false;
    }

    pub fn refresh_addr(&mut self, node_id: &NodeId, addr: A) {
        if let Some(node) = self.nodes.get_mut(node_id) {
            node.refresh_addr(addr);
            self.last_changed = Instant::now();
        }
    }

    pub fn fail_addr(&mut self, node_id: &NodeId, addr: A) {
        if let Some(node) = self.nodes.get_mut(node_id) {
            node.fail_addr(addr);
        }
    }
}

impl<A> Default for Bucket<A> {
    fn default() -> Self {
        Self {
            nodes: HashMap::new(),
            last_changed: Instant::now(),
        }
    }
}

#[derive(Debug)]
struct RoutingTable<A> {
    id: NodeId,
    buckets: [Bucket<A>; 160],
}

impl<A: std::hash::Hash + Eq + Copy> RoutingTable<A> {
    fn new(id: NodeId) -> Self {
        Self {
            id,
            buckets: array::from_fn(|_| Bucket::default()),
        }
    }

    fn get_bucket_mut(&mut self, node_id: &NodeId) -> Option<&mut Bucket<A>> {
        let prefix_length = node_id.kademlia(&self.id).prefix_length();
        if prefix_length == 160 {
            return None;
        }
        Some(&mut self.buckets[prefix_length as usize])
    }

    pub fn add_addr(&mut self, id: NodeId, addr: A) -> Option<u8> {
        let prefix_length = id.kademlia(&self.id).prefix_length();
        if prefix_length == 160 {
            return None;
        }
        self.buckets[prefix_length as usize]
            .add_addr(id, addr)
            .then_some(prefix_length)
    }

    pub fn get_maintainable(&self, num: usize) -> Vec<u8> {
        let maintainable: Vec<_> = self
            .buckets
            .iter()
            .enumerate()
            .filter(|(_, b)| b.is_maintainable())
            .map(|(b, _)| b as u8)
            .collect();
        let mut scored: Vec<_> = maintainable
            .into_iter()
            .map(|m| {
                (
                    m,
                    (8 - self.buckets[m as usize].len()) + rand::rng().random_range(0..18),
                )
            })
            .collect();
        scored.sort_unstable_by_key(|&(_, score)| score);
        scored.into_iter().map(|(m, _)| m).rev().take(num).collect()
    }

    pub fn get_closest_nodes(&self, id: &impl AsRef<[u8; 20]>, num: usize) -> HashMap<NodeId, A> {
        let mut nodes: Vec<_> = self.buckets.iter().flat_map(|b| b.nodes.values()).collect();
        nodes.sort_unstable_by_key(|n| n.id.kademlia(id));
        nodes
            .iter()
            .filter_map(|n| n.best_addr().map(|a| (n.id, a)))
            .take(num)
            .collect()
    }

    pub fn get_closest_addrs(&self, id: &impl AsRef<[u8; 20]>, num: usize) -> HashSet<A> {
        self.get_closest_nodes(id, num).into_values().collect()
    }

    pub fn refresh_addr(&mut self, node_id: &NodeId, addr: A) {
        self.get_bucket_mut(node_id)
            .map(|b| b.refresh_addr(node_id, addr));
    }

    pub fn fail_addr(&mut self, node_id: &NodeId, addr: A) {
        self.get_bucket_mut(node_id)
            .map(|b| b.fail_addr(node_id, addr));
    }
}

#[derive(Debug)]
struct InfoHashEntry {
    visited_ids: HashSet<(NodeId, bool)>,
    peers: HashSet<SocketAddr>,
    tx: Option<mpsc::Sender<Event>>,
}

impl Default for InfoHashEntry {
    fn default() -> Self {
        Self {
            visited_ids: HashSet::new(),
            peers: HashSet::new(),
            tx: None,
        }
    }
}

impl InfoHashEntry {
    pub fn new(tx: mpsc::Sender<Event>) -> Self {
        Self {
            visited_ids: HashSet::new(),
            peers: HashSet::new(),
            tx: Some(tx),
        }
    }
}

pub enum DHTEvent {
    FoundNode(HashSet<SocketAddr>),
    InfoHashRequest(Hash, mpsc::Sender<Event>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Secret([u8; 20]);
impl Secret {
    fn random() -> Self {
        Self(rng().random())
    }

    fn token(&self, addr: &SocketAddr) -> Self {
        match addr {
            SocketAddr::V4(v4) => {
                let mut buff = [0; 26];
                buff[..20].copy_from_slice(&self.0);
                buff[20..].copy_from_slice(&v4.to_compact());
                Self(sha1::Sha1::digest(buff).into())
            }
            SocketAddr::V6(v6) => {
                let mut buff = [0; 38];
                buff[..20].copy_from_slice(&self.0);
                buff[20..].copy_from_slice(&v6.to_compact());
                Self(sha1::Sha1::digest(buff).into())
            }
        }
    }

    fn into_vec(self) -> Vec<u8> {
        Vec::from(self.0)
    }
}

pub struct DHTManager {
    id: NodeId,
    dht: DHT,
    routing_table_4: RoutingTable<SocketAddrV4>,
    routing_table_6: RoutingTable<SocketAddrV6>,
    rx: mpsc::Receiver<DHTEvent>,
    pub tx: mpsc::Sender<DHTEvent>,
    pub info_rx: watch::Receiver<DHTInfo>,
    config_rx: watch::Receiver<Config>,
    cancel: CancellationToken,
    info_tx: watch::Sender<DHTInfo>,

    info_hashes: HashMap<Hash, InfoHashEntry>,
    discovered_ids: HashMap<NodeId, HashSet<NodeId>>,
    current_secret: Secret,
    previous_secret: Option<Secret>,
}

impl DHTManager {
    const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(1);
    const SECRET_INTERVAL: Duration = Duration::from_mins(5);

    pub async fn new(
        cancel: CancellationToken,
        config_rx: watch::Receiver<Config>,
    ) -> Result<Self> {
        let (tx, rx) = mpsc::channel(100);
        let (info_tx, info_rx) = watch::channel(DHTInfo::default());
        let node_id = config_rx.borrow().node_id;
        let dht_port = config_rx.borrow().dht_port;
        Ok(Self {
            dht: DHT::bind(node_id, dht_port).await?,
            routing_table_4: RoutingTable::new(node_id),
            routing_table_6: RoutingTable::new(node_id),
            id: node_id,
            tx,
            rx,
            cancel,
            info_tx,
            info_rx,
            config_rx,

            info_hashes: HashMap::new(),
            discovered_ids: HashMap::new(),
            current_secret: Secret::random(),
            previous_secret: None,
        })
    }

    pub async fn run(mut self) -> Result<()> {
        let span = info_span!("DHT");
        let _enter = span.enter();
        async {
            let mut secret_interval = tokio::time::interval(Self::SECRET_INTERVAL);
            let mut maintenance_interval = tokio::time::interval(Self::MAINTENANCE_INTERVAL);
            maintenance_interval.tick().await;
            loop {
                select! {
                    _ = maintenance_interval.tick() => {
                        self.maintain().await?;
                    },
                    _ = secret_interval.tick() => {
                        warn!("{:?}", self.info_hashes);
                        self.previous_secret = Some(self.current_secret);
                        self.current_secret = Secret::random();
                    },
                    _ = self.cancel.cancelled() => {
                        break;
                    },
                    message = self.dht.receive() => match message {
                        Ok((addr, message, query)) => self.handle_message(addr, message, query).await?,
                        Err(e) => {
                            if let Some(e) = e.downcast_ref::<BencodeError>()
                                && matches!(e.kind, BencodeErrorKind::InvalidValue { .. })
                            {
                                continue;
                            }
                            warn!("Failed to receive/decode DHT message: {e:#}");
                        }
                    },
                    event = self.rx.recv() => match event {
                        Some(event) => {
                            let cancel = self.cancel.clone();
                            select! {
                                _ = cancel.cancelled() => break,
                                result = self.handle_event(event) => result?,
                            }
                        },
                        None => break,
                    }
                };
            }
            Ok(())
        }.instrument(span.clone()).await
    }

    async fn maintain(&mut self) -> Result<()> {
        for inactive in self.routing_table_4.get_maintainable(5) {
            let target = self.id.random_with_prefix_length(inactive);
            debug!("Performing maintenance for {target} of {}", self.id);
            self.discovered_ids.remove(&target);
            for addr in self
                .routing_table_4
                .get_closest_addrs(&target, 8)
                .into_iter()
                .map(SocketAddr::V4)
            {
                let _ = self
                    .dht
                    .query(
                        addr,
                        Query::FindNode {
                            target,
                            want_nodes: WantNodes::new(true, true),
                        },
                    )
                    .await;
            }
        }
        for inactive in self.routing_table_6.get_maintainable(5) {
            let target = self.id.random_with_prefix_length(inactive);
            debug!("Performing maintenance for {target} of {}", self.id);
            self.discovered_ids.remove(&target);
            for addr in self
                .routing_table_6
                .get_closest_addrs(&target, 8)
                .into_iter()
                .map(SocketAddr::V6)
            {
                let _ = self
                    .dht
                    .query(
                        addr,
                        Query::FindNode {
                            target,
                            want_nodes: WantNodes::new(true, true),
                        },
                    )
                    .await;
            }
        }
        Ok(())
    }

    fn closest_nodes(
        &self,
        target: &impl AsRef<[u8; 20]>,
        want_nodes: WantNodes,
        v6: bool,
    ) -> Vec<NodeInfo> {
        let mut nodes = Vec::new();
        if want_nodes.nodes4 || (!want_nodes.nodes4 && !want_nodes.nodes6 && !v6) {
            nodes.extend(
                self.routing_table_4
                    .get_closest_nodes(target, 8)
                    .into_iter()
                    .map(|(id, addr)| (id, SocketAddr::V4(addr))),
            );
        }
        if want_nodes.nodes6 || (!want_nodes.nodes4 && !want_nodes.nodes6 && v6) {
            nodes.extend(
                self.routing_table_6
                    .get_closest_nodes(target, 8)
                    .into_iter()
                    .map(|(id, addr)| (id, SocketAddr::V6(addr))),
            );
        }
        nodes
            .into_iter()
            .map(|(id, addr)| NodeInfo::new(id, addr))
            .collect()
    }

    async fn handle_message(
        &mut self,
        mut addr: SocketAddr,
        message: Message,
        query: Option<Query>,
    ) -> Result<()> {
        match message.type_of {
            MessageType::Query { id, query } => match query {
                Query::Ping => {
                    let _ = self
                        .dht
                        .respond(&addr, message.transaction_id, Response::PING)
                        .await;
                }
                Query::FindNode { target, want_nodes } => {
                    let nodes = self.closest_nodes(&target, want_nodes, addr.is_ipv6());
                    let _ = self
                        .dht
                        .respond(&addr, message.transaction_id, Response::find_nodes(nodes))
                        .await;
                }
                Query::GetPeers {
                    info_hash,
                    want_nodes,
                } => {
                    let token = self.current_secret.token(&addr);
                    if let Some(knowledge) = self.info_hashes.get(&info_hash) {
                        let _ = self
                            .dht
                            .respond(
                                &addr,
                                message.transaction_id,
                                Response::get_peers_values(
                                    token.into_vec(),
                                    knowledge.peers.clone(),
                                ),
                            )
                            .await;
                    } else {
                        let nodes = self.closest_nodes(&info_hash, want_nodes, addr.is_ipv6());
                        let _ = self
                            .dht
                            .respond(
                                &addr,
                                message.transaction_id,
                                Response::get_peers_nodes(token.into_vec(), nodes),
                            )
                            .await;
                    }
                }
                Query::AnnouncePeer {
                    implied_port,
                    info_hash,
                    port,
                    token,
                } => {
                    if token
                        .try_into()
                        .map(|token| {
                            let token = Secret(token);
                            token != self.current_secret.token(&addr)
                                && self
                                    .previous_secret
                                    .map(|p| p.token(&addr) != token)
                                    .unwrap_or(true)
                        })
                        .unwrap_or(true)
                    {
                        let _ = self
                            .dht
                            .error(
                                &addr,
                                message.transaction_id,
                                ErrorCode::Generic,
                                String::from("Invalid token"),
                            )
                            .await;
                        return Ok(());
                    }
                    let entry = self.info_hashes.entry(info_hash).or_default();
                    if !implied_port {
                        addr.set_port(port);
                    }
                    entry.peers.insert(addr);
                }
                _ => {
                    warn!("Got query {id} {addr}: {query:?}");
                }
            },
            MessageType::Response { response, id } => {
                match addr {
                    SocketAddr::V4(v4) => self.routing_table_4.refresh_addr(&id, v4),
                    SocketAddr::V6(v6) => self.routing_table_6.refresh_addr(&id, v6),
                }
                match query.unwrap() {
                    Query::FindNode { target, want_nodes } => {
                        if let Some(nodes) = response.nodes {
                            self.find_nodes(id, addr, target, nodes, response.token)
                                .await?;
                        }
                    }
                    Query::GetPeers { info_hash, .. } => {
                        if let Some(nodes) = response.nodes {
                            self.get_peers(info_hash, id, addr, nodes).await?;
                        }
                        if let Some(addrs) = response.values {
                            debug!("BINGO! {addrs:?}");
                            if let Some(entry) = self.info_hashes.get_mut(&info_hash) {
                                entry.peers.extend(addrs.clone());
                                if let Some(tx) = &entry.tx {
                                    let _ = tx.send(Event::DHT(addrs)).await;
                                }
                            }
                        }
                    }
                    _ => (),
                }
            }
            _ => (),
        }
        Ok(())
    }

    fn add_addr(&mut self, id: NodeId, addr: SocketAddr) {
        match addr {
            SocketAddr::V4(v4) => {
                if let Some(prefix_length) = self.routing_table_4.add_addr(id, v4) {
                    self.info_tx.send_modify(|info| {
                        info.buckets_4[prefix_length as usize]
                            .nodes
                            .push(DHTNodeInfo {});
                    });
                }
            }
            SocketAddr::V6(v6) => {
                if let Some(prefix_length) = self.routing_table_6.add_addr(id, v6) {
                    self.info_tx.send_modify(|info| {
                        info.buckets_6[prefix_length as usize]
                            .nodes
                            .push(DHTNodeInfo {});
                    });
                }
            }
        };
    }

    async fn find_nodes(
        &mut self,
        id: NodeId,
        addr: SocketAddr,
        target: NodeId,
        nodes: Vec<NodeInfo>,
        token: Option<Vec<u8>>,
    ) -> Result<()> {
        let kademlia = id.kademlia(&target);
        let prefix_length = kademlia.prefix_length();
        if prefix_length == 160 && target == self.id {
            return Ok(());
        }

        let mut closest = true;
        self.add_addr(id, addr.clone());
        for info in nodes {
            if !self
                .discovered_ids
                .entry(target)
                .or_default()
                .contains(&info.id)
            {
                self.discovered_ids.entry(target).and_modify(|t| {
                    t.insert(info.id);
                });
                let other_kademlia = info.id.kademlia(&target);
                let other_prefix_length = other_kademlia.prefix_length();
                if other_prefix_length == 160 && target == self.id {
                    continue;
                }
                if other_kademlia < kademlia {
                    for addr in info.addrs {
                        let _ = self
                            .dht
                            .query(
                                addr,
                                Query::FindNode {
                                    target,
                                    want_nodes: WantNodes::new(true, true),
                                },
                            )
                            .await;
                    }
                    closest = false;
                }
            }
        }

        if let Some(token) = token
            && closest
        {
            let bit_torrent_port = self.config_rx.borrow().bit_torrent_port;
            self.dht
                .query(
                    addr,
                    Query::AnnouncePeer {
                        implied_port: false,
                        info_hash: Hash(target.into()),
                        port: bit_torrent_port,
                        token,
                    },
                )
                .await?;
        }
        Ok(())
    }

    async fn get_peers(
        &mut self,
        info_hash: Hash,
        id: NodeId,
        addr: SocketAddr,
        nodes: Vec<NodeInfo>,
    ) -> Result<()> {
        let v6 = addr.is_ipv6();
        self.add_addr(id, addr);

        let kademlia = id.kademlia(&info_hash);
        let prefix_length = kademlia.prefix_length();
        if prefix_length == 160 {
            return Ok(());
        }

        self.info_hashes
            .get_mut(&info_hash)
            .map(|l| l.visited_ids.insert((id, v6)));
        for info in nodes {
            if !self
                .info_hashes
                .get(&info_hash)
                .map(|l| l.visited_ids.contains(&(info.id, v6)))
                .unwrap_or(true)
            {
                self.info_hashes
                    .get_mut(&info_hash)
                    .map(|l| l.visited_ids.insert((info.id, v6)));
                let other_kademlia = info.id.kademlia(&info_hash);
                let other_prefix_length = other_kademlia.prefix_length();
                if other_prefix_length == 160 {
                    continue;
                }
                if other_kademlia < kademlia {
                    for addr in info.addrs {
                        let _ = self
                            .dht
                            .query(
                                addr,
                                Query::GetPeers {
                                    info_hash,
                                    want_nodes: WantNodes::new(true, true),
                                },
                            )
                            .await;
                    }
                }
            }
        }
        Ok(())
    }

    async fn handle_event(&mut self, event: DHTEvent) -> Result<()> {
        use DHTEvent::*;
        match event {
            FoundNode(addrs) => {
                debug!("Received DHT node {addrs:?}");
                for addr in addrs {
                    let _ = self
                        .dht
                        .query(
                            addr,
                            Query::FindNode {
                                target: self.id,
                                want_nodes: WantNodes::new(true, true),
                            },
                        )
                        .await;
                }
            }
            InfoHashRequest(info_hash, tx) => {
                debug!("Received info hash request {info_hash}");
                self.info_hashes.insert(info_hash, InfoHashEntry::new(tx));
                for addr in self
                    .routing_table_4
                    .get_closest_addrs(&info_hash, 8)
                    .into_iter()
                    .map(SocketAddr::V4)
                {
                    let _ = self
                        .dht
                        .query(
                            addr,
                            Query::GetPeers {
                                info_hash,
                                want_nodes: WantNodes::new(true, false),
                            },
                        )
                        .await;
                }
                for addr in self
                    .routing_table_6
                    .get_closest_addrs(&info_hash, 8)
                    .into_iter()
                    .map(SocketAddr::V6)
                {
                    let _ = self
                        .dht
                        .query(
                            addr,
                            Query::GetPeers {
                                info_hash,
                                want_nodes: WantNodes::new(false, true),
                            },
                        )
                        .await;
                }
            }
        }
        Ok(())
    }
}
