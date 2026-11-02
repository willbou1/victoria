use anyhow::Result;
use bencodedit::{BencodeError, BencodeErrorKind};
use rand::{RngExt, seq::SliceRandom};
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
use tracing::{Instrument, debug, info_span};

use super::{
    proto::dht::*,
    torrent::{
        Event,
        control::{DHTInfo, DHTNodeInfo},
    },
    types::*,
};

const MAX_NUM_NODES_PER_BUCKET: usize = 8;
const GOOD_INTERVAL: Duration = Duration::from_mins(1);
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug)]
struct Node {
    info: NodeInfo,
    last_sent: Instant,
    num_failed_queries: usize,
}

impl Node {
    pub fn new(info: NodeInfo) -> Self {
        Self {
            info,
            last_sent: Instant::now(),
            num_failed_queries: 0,
        }
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

    pub fn add_node(&mut self, info: &NodeInfo) -> bool {
        if self.nodes.contains_key(&info.id) {
            self.refresh_node(&info.id);
            return false;
        }
        if !self.is_full() {
            self.last_changed = Instant::now();
            self.nodes.insert(info.id, Node::new(info.clone()));
            return true;
        }
        return false;
    }

    pub fn refresh_node(&mut self, node_id: &NodeId) {
        if let Some(node) = self.nodes.get_mut(node_id) {
            node.last_sent = Instant::now();
            self.last_changed = Instant::now();
        }
    }

    pub fn fail_node(&mut self, node_id: &NodeId) {
        if let Some(node) = self.nodes.get_mut(node_id) {
            node.num_failed_queries += 1;
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

    pub fn add_node(&mut self, info: &NodeInfo) -> Option<u8> {
        let prefix_length = info.id.kademlia(&self.id).prefix_length();
        if prefix_length == 160 {
            return None;
        }
        self.buckets[prefix_length as usize]
            .add_node(info)
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
                    (8 - self.buckets[m as usize].len()) + rand::rng().random_range(0..16),
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
            .min_by_key(|n| n.info.id.kademlia(id))
    }

    pub fn get_closest_nodes(&self, id: &impl AsRef<[u8; 20]>, num: usize) -> Vec<&Node> {
        let mut nodes: Vec<_> = self.buckets.iter().flat_map(|b| b.nodes.values()).collect();
        nodes.sort_unstable_by_key(|n| n.info.id.kademlia(id));
        nodes.into_iter().take(num).collect()
    }

    pub fn refresh_node(&mut self, node_id: &NodeId) {
        self.get_bucket_mut(node_id)
            .map(|b| b.refresh_node(node_id));
    }

    pub fn fail_node(&mut self, node_id: &NodeId) {
        self.get_bucket_mut(node_id).map(|b| b.fail_node(node_id));
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
    FoundNode(Endpoint),
    InfoHashRequest(Hash, mpsc::Sender<Event>),
}

pub struct DHTManager {
    id: NodeId,
    dht: DHT,
    routing_table: RoutingTable,
    rx: mpsc::Receiver<DHTEvent>,
    pub tx: mpsc::Sender<DHTEvent>,
    pub info_rx: watch::Receiver<DHTInfo>,
    info_tx: watch::Sender<DHTInfo>,
    cancellation_token: CancellationToken,

    info_hashes: HashMap<Hash, InfoHashKnowledge>,
    discovered_ids: HashMap<NodeId, HashSet<NodeId>>,
}

impl DHTManager {
    pub async fn new(cancellation_token: CancellationToken) -> Result<Self> {
        let (tx, rx) = mpsc::channel(100);
        let (info_tx, info_rx) = watch::channel(DHTInfo::default());
        let node_id = NodeId::random();
        Ok(Self {
            dht: DHT::bind(node_id, 6767).await?,
            routing_table: RoutingTable::new(node_id),
            id: node_id,
            tx,
            rx,
            info_tx,
            info_rx,
            cancellation_token,

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
                    _ = self.cancellation_token.cancelled() => {
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
                            debug!("Failed to receive/decode DHT message: {e:#}");
                        }
                    },
                    event = self.rx.recv() => match event {
                        Some(event) => {
                            let cancel = self.cancellation_token.clone();
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
                    .query(&node.info.endpoint, Query::FindNode { target })
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
            MessageType::Response { response, id } => {
                self.routing_table.refresh_node(&id);
                match query.unwrap() {
                    Query::FindNode { target } => {
                        if let Some(nodes) = response.nodes {
                            self.find_nodes(id, endpoint, target, nodes).await?;
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
    ) -> Result<()> {
        let kademlia = id.kademlia(&target);
        let prefix_length = kademlia.prefix_length();
        if prefix_length == 160 && target == self.id {
            return Ok(());
        }

        self.add_node(NodeInfo { id, endpoint });
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
                        .query(&info.endpoint, Query::FindNode { target })
                        .await;
                }
            }
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
        self.add_node(NodeInfo { id, endpoint });

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
                        .query(&info.endpoint, Query::GetPeers { info_hash })
                        .await;
                }
            }
        }
        Ok(())
    }

    async fn handle_event(&mut self, event: DHTEvent) -> Result<()> {
        use DHTEvent::*;
        match event {
            FoundNode(endpoint) => {
                debug!("Received DHT node {endpoint}");
                let _ = self
                    .dht
                    .query(&endpoint, Query::FindNode { target: self.id })
                    .await;
            }
            InfoHashRequest(info_hash, tx) => {
                debug!("Received info hash request {info_hash}");
                self.info_hashes
                    .insert(info_hash, InfoHashKnowledge::new(tx));
                for node in self.routing_table.get_closest_nodes(&info_hash, 8) {
                    let _ = self
                        .dht
                        .query(&node.info.endpoint, Query::GetPeers { info_hash })
                        .await;
                }
            }
            _ => (),
        }
        Ok(())
    }
}
