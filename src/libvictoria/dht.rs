use anyhow::Result;
use bencodedit::{BencodeError, BencodeErrorKind};
use rand::RngExt;
use std::{
    array,
    collections::{HashMap, HashSet},
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

const MAX_NUM_NODES_PER_BUCKET: usize = 8;
const GOOD_INTERVAL: Duration = Duration::from_mins(1);
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(1);

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
struct Node {
    id: NodeId,
    pub endpoints: HashMap<Endpoint, EndpointStatistics>,
}

impl Node {
    pub fn new(id: NodeId, endpoint: Endpoint) -> Self {
        Self {
            id,
            endpoints: HashMap::from([(endpoint, EndpointStatistics::default())]),
        }
    }

    pub fn add_endpoint(&mut self, endpoint: Endpoint) {
        self.endpoints.entry(endpoint).or_default();
    }

    pub fn refresh_endpoint(&mut self, endpoint: &Endpoint) {
        self.endpoints
            .get_mut(endpoint)
            .map(|e| e.last_sent = Instant::now());
    }

    pub fn fail_endpoint(&mut self, endpoint: &Endpoint) {
        self.endpoints
            .get_mut(endpoint)
            .map(|e| e.num_failed_queries += 1);
    }
}

#[derive(Debug)]
struct Bucket {
    nodes: HashMap<NodeId, Node>,
    last_changed: Instant,
}

impl Bucket {
    pub fn is_full(&self) -> bool {
        self.nodes.len() >= MAX_NUM_NODES_PER_BUCKET
    }

    pub fn is_maintainable(&self) -> bool {
        !self.is_full()
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn add_endpoint(&mut self, id: NodeId, endpoint: Endpoint) -> bool {
        if let Some(node) = self.nodes.get_mut(&id) {
            self.refresh_node(&info.id, &endpoint);
            node.add_endpoint(endpoint);
            return false;
        }
        if !self.is_full() {
            self.last_changed = Instant::now();
            self.nodes.insert(id, Node::new(id, endpoint));
            return true;
        }
        return false;
    }

    pub fn refresh_endpoint(&mut self, node_id: &NodeId, endpoint: &Endpoint) {
        if let Some(node) = self.nodes.get_mut(node_id) {
            node.refresh_endpoint(endpoint);
            self.last_changed = Instant::now();
        }
    }

    pub fn fail_endpoint(&mut self, node_id: &NodeId, endpoint: &Endpoint) {
        if let Some(node) = self.nodes.get_mut(node_id) {
            node.fail_endpoint(endpoint);
        }
    }
}

impl Default for Bucket {
    fn default() -> Self {
        Self {
            nodes: HashMap::new(),
            last_changed: Instant::now(),
        }
    }
}

#[derive(Debug)]
struct RoutingTable {
    id: NodeId,
    buckets: [Bucket; 160],
}

impl RoutingTable {
    fn new(id: NodeId) -> Self {
        Self {
            id,
            buckets: array::from_fn(|_| Bucket::default()),
        }
    }

    fn get_bucket_mut(&mut self, node_id: &NodeId) -> Option<&mut Bucket> {
        let prefix_length = node_id.kademlia(&self.id).prefix_length();
        if prefix_length == 160 {
            return None;
        }
        Some(&mut self.buckets[prefix_length as usize])
    }

    pub fn add_endpoint(&mut self, id: NodeId, endpoint: Endpoint) -> Option<u8> {
        let prefix_length = id.kademlia(&self.id).prefix_length();
        if prefix_length == 160 {
            return None;
        }
        self.buckets[prefix_length as usize]
            .add_endpoint(id, endpoint)
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

    pub fn get_closest_node(&self, id: &impl AsRef<[u8; 20]>) -> Option<&Node> {
        self.buckets
            .iter()
            .flat_map(|b| b.nodes.values())
            .min_by_key(|n| n.id.kademlia(id))
    }

    pub fn get_closest_endpoints(
        &self,
        id: &impl AsRef<[u8; 20]>,
        want: WantNodes,
        num: usize,
    ) -> Vec<&Endpoint> {
        let mut nodes: Vec<_> = self
            .buckets
            .iter()
            .flat_map(|b| b.nodes.values())
            .filter(|n| {
                n.endpoints
                    .keys()
                    .any(|e| (want.nodes6 && e.is_v6()) || (want.nodes4 && e.is_v4()))
            })
            .collect();
        nodes.sort_unstable_by_key(|n| n.id.kademlia(id));
        nodes.into_iter().take(num).collect()
    }

    pub fn refresh_endpoint(&mut self, node_id: &NodeId, endpoint: &Endpoint) {
        self.get_bucket_mut(node_id)
            .map(|b| b.refresh_endpoint(node_id, endpoint));
    }

    pub fn fail_endpoint(&mut self, node_id: &NodeId, endpoint: &Endpoint) {
        self.get_bucket_mut(node_id)
            .map(|b| b.fail_endpoint(node_id, endpoint));
    }
}

struct InfoHashKnowledge {
    visited_ids: HashSet<NodeId>,
    peers: HashSet<EndpointV4>,
    tx: mpsc::Sender<Event>,
}

impl InfoHashKnowledge {
    pub fn new(tx: mpsc::Sender<Event>) -> Self {
        Self {
            visited_ids: HashSet::new(),
            peers: HashSet::new(),
            tx,
        }
    }
}

pub enum DHTEvent {
    FoundNode(EndpointSet),
    InfoHashRequest(Hash, mpsc::Sender<Event>),
}

pub struct DHTManager {
    id: NodeId,
    dht: DHT,
    routing_table: RoutingTable,
    rx: mpsc::Receiver<DHTEvent>,
    pub tx: mpsc::Sender<DHTEvent>,
    pub info_rx: watch::Receiver<DHTInfo>,
    config_rx: watch::Receiver<Config>,
    cancel: CancellationToken,
    info_tx: watch::Sender<DHTInfo>,

    info_hashes: HashMap<Hash, InfoHashKnowledge>,
    discovered_ids: HashMap<NodeId, HashSet<NodeId>>,
}

impl DHTManager {
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
            routing_table: RoutingTable::new(node_id),
            id: node_id,
            tx,
            rx,
            cancel,
            info_tx,
            info_rx,
            config_rx,

            info_hashes: HashMap::new(),
            discovered_ids: HashMap::new(),
        })
    }

    pub async fn run(mut self) -> Result<()> {
        let span = info_span!("DHT");
        let _enter = span.enter();
        async {
            let mut maintenance_interval = tokio::time::interval(MAINTENANCE_INTERVAL);
            maintenance_interval.tick().await;
            loop {
                select! {
                    _ = maintenance_interval.tick() => {
                        self.maintain().await?;
                    },
                    _ = self.cancel.cancelled() => {
                        break;
                    },
                    message = self.dht.receive() => match message {
                        Ok((endpoint, message, query)) => self.handle_message(endpoint, message, query).await?,
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
        for inactive in self.routing_table.get_maintainable(5) {
            let target = self.id.random_with_prefix_length(inactive);
            debug!("Performing maintenance for {target} of {}", self.id);
            self.discovered_ids.remove(&target);
            for node in self.routing_table.get_closest_nodes(&target, 8) {
                let _ = self
                    .dht
                    .query(
                        &node.info.endpoint,
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

    async fn handle_message(
        &mut self,
        endpoint: Endpoint,
        message: Message,
        query: Option<Query>,
    ) -> Result<()> {
        match message.type_of {
            MessageType::Query { id, query } => {
                warn!("Got query {id} {endpoint}: {query:?}");
            }
            MessageType::Response { response, id } => {
                self.routing_table.refresh_node(&id);
                match query.unwrap() {
                    Query::FindNode { target, want_nodes } => {
                        if let Some(nodes) = response.nodes {
                            self.find_nodes(id, endpoint, target, nodes, response.token)
                                .await?;
                        }
                    }
                    Query::GetPeers { info_hash, .. } => {
                        if let Some(nodes) = response.nodes {
                            self.get_peers(info_hash, id, endpoint, nodes).await?;
                        }
                        if let Some(peers) = response.values {
                            debug!("BINGO! {peers:?}");
                            if let Some(knowledge) = self.info_hashes.get_mut(&info_hash) {
                                knowledge.peers.extend(peers.clone());
                                let _ = knowledge.tx.send(Event::DHT(peers)).await;
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

    fn add_node(&mut self, info: NodeInfo) {
        if let Some(prefix_length) = self.routing_table.add_node(&info) {
            self.info_tx.send_modify(|info| {
                info.buckets[prefix_length as usize]
                    .nodes
                    .push(DHTNodeInfo {});
            });
        }
    }

    async fn find_nodes(
        &mut self,
        id: NodeId,
        endpoint: Endpoint,
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
        self.add_endpoint(id, endpoint.clone());
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
                    let _ = self
                        .dht
                        .query(
                            &info.endpoint,
                            Query::FindNode {
                                target,
                                want_nodes: WantNodes::new(true, true),
                            },
                        )
                        .await;
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
                    &endpoint,
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
        endpoint: Endpoint,
        nodes: Vec<NodeInfo>,
    ) -> Result<()> {
        self.add_endpoint(id, endpoint);

        let kademlia = id.kademlia(&info_hash);
        let prefix_length = kademlia.prefix_length();
        if prefix_length == 160 {
            return Ok(());
        }

        self.info_hashes
            .get_mut(&info_hash)
            .map(|l| l.visited_ids.insert(id));
        for info in nodes {
            if !self
                .info_hashes
                .get(&info_hash)
                .map(|l| l.visited_ids.contains(&info.id))
                .unwrap_or(true)
            {
                self.info_hashes
                    .get_mut(&info_hash)
                    .map(|l| l.visited_ids.insert(info.id));
                let other_kademlia = info.id.kademlia(&info_hash);
                let other_prefix_length = other_kademlia.prefix_length();
                if other_prefix_length == 160 {
                    continue;
                }
                if other_kademlia < kademlia {
                    let _ = self
                        .dht
                        .query(
                            &info.endpoint,
                            Query::GetPeers {
                                info_hash,
                                want_nodes: WantNodes::new(true, true),
                            },
                        )
                        .await;
                }
            }
        }
        Ok(())
    }

    async fn handle_event(&mut self, event: DHTEvent) -> Result<()> {
        use DHTEvent::*;
        match event {
            FoundNode(endpoints) => {
                debug!("Received DHT node {endpoints}");
                for endpoint in &endpoints {
                    let _ = self
                        .dht
                        .query(
                            &endpoint,
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
                self.info_hashes
                    .insert(info_hash, InfoHashKnowledge::new(tx));
                for node in self.routing_table.get_closest_nodes(&info_hash, 8) {
                    let _ = self
                        .dht
                        .query(
                            &node.info.endpoint,
                            Query::GetPeers {
                                info_hash,
                                want_nodes: WantNodes::new(true, true),
                            },
                        )
                        .await;
                }
            }
        }
        Ok(())
    }
}
