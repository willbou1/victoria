use anyhow::Result;
use tokio::{
    select, sync::{mpsc, watch}
};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, info_span, debug};
use std::{
    array,
    time::{Duration, Instant},
    collections::{HashSet, HashMap},
};
use bencodedit::{BencodeError, BencodeErrorKind};

use super::{
    proto::dht::*,
    torrent::{Event, control::{DHTInfo, DHTNodeInfo}},
    types::*,
};

const MAX_NUM_NODES_PER_BUCKET: usize = 8; 
const GOOD_INTERVAL: Duration = Duration::from_mins(15);
const REFRESH_INTERVAL: Duration = Duration::from_secs(20);

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
    nodes: Vec<Node>,
    last_changed: Instant,
}

impl Bucket {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            last_changed: Instant::now(),
        }
    }

    pub fn is_full(&self) -> bool {
        self.nodes.len() >= MAX_NUM_NODES_PER_BUCKET
    }
}

struct InfoHashLookup {
    visited_ids: HashSet<NodeId>,
    peers: HashSet<EndpointV4>,
    tx: mpsc::Sender<Event>,
}

impl InfoHashLookup {
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
    buckets: [Bucket; 160],
    rx: mpsc::Receiver<DHTEvent>,
    pub tx: mpsc::Sender<DHTEvent>,
    pub info_rx: watch::Receiver<DHTInfo>,
    info_tx: watch::Sender<DHTInfo>,
    cancellation_token: CancellationToken,

    info_hashes: HashMap<Hash, InfoHashLookup>,
    discovered_ids: HashSet<NodeId>,
}

impl DHTManager {
    pub async fn new(cancellation_token: CancellationToken) -> Result<Self> {
        let (tx, rx) = mpsc::channel(100);
        let (info_tx, info_rx) = watch::channel(DHTInfo::default());
        let node_id = NodeId::random();
        Ok(Self {
            dht: DHT::bind(node_id, 6767).await?,
            id: node_id,
            buckets: array::from_fn(|_| Bucket::new()),
            tx,
            rx,
            info_tx,
            info_rx,
            cancellation_token,
            
            info_hashes: HashMap::new(),
            discovered_ids: HashSet::new(),
        })
    }

    pub async fn run(mut self) -> Result<()> {
        let span = info_span!("DHT");
        let _enter = span.enter();
        async {
            let mut refresh_interval = tokio::time::interval(REFRESH_INTERVAL);
            refresh_interval.tick().await;
            loop {
                select! {
                    _ = refresh_interval.tick() => {
                        debug!("{:?}", self.buckets);
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

    pub async fn handle_message(
        &mut self,
        endpoint: Endpoint,
        message: Message,
        query: Option<Query>
    ) -> Result<()> {
        match message.type_of {
            MessageType::Response {response, id} => match query.unwrap() {
                Query::FindNode {..} => {
                    if let Some(nodes) = response.nodes {
                        self.find_nodes(&id, endpoint, &nodes).await?;
                    }
                }
                Query::GetPeers { info_hash, .. } => {
                    if let Some(nodes) = response.nodes {
                        self.get_peers(info_hash, &id, endpoint, &nodes).await?;
                    }
                    if let Some(peers) = response.values {
                        debug!("BINGO! {peers:?}");
                        if let Some(lookup) = self.info_hashes.get_mut(&info_hash) {
                            lookup.peers.extend(peers.clone());
                            let _ = lookup.tx.send(Event::DHT(peers)).await;
                        }
                    }
                }
                _ => (),
            }
            _ => (),
        }
        Ok(())
    }

    pub fn add_node(&mut self, prefix_length: u8, info: NodeInfo) {
        let bucket = &mut self.buckets[prefix_length as usize];
        if !bucket.is_full() {
            bucket.nodes.push(Node::new(info));
            self.info_tx.send_modify(|info| {
                info.buckets[prefix_length as usize].nodes.push(DHTNodeInfo {});
            });
        }
    }

    async fn find_nodes(&mut self, id: &NodeId, endpoint: Endpoint, nodes: &[NodeInfo]) -> Result<()> {
        let prefix_length = id.kademlia(&self.id).prefix_length();
        if prefix_length == 160 {
            return Ok(());
        }

        self.add_node(prefix_length, NodeInfo {
            id: *id,
            endpoint,
        });
        for info in nodes {
            if !self.discovered_ids.contains(&info.id) {
                self.discovered_ids.insert(info.id);
                let other_prefix_length = info.id.kademlia(&self.id)
                    .prefix_length();
                if other_prefix_length == 160 {
                    continue;
                }
                if other_prefix_length > prefix_length {
                    let _ = self.dht.query(&info.endpoint, Query::FindNode {
                        target: self.id
                    }).await;
                }
            }
        }
        Ok(())
    }

    async fn get_peers(
        &mut self,
        info_hash: Hash,
        id: &NodeId,
        endpoint: Endpoint,
        nodes: &[NodeInfo]
    ) -> Result<()> {
        let prefix_length = id.kademlia(&info_hash).prefix_length();
        if prefix_length == 160 {
            return Ok(());
        }

        self.info_hashes.get_mut(&info_hash).map(|l| l.visited_ids.insert(*id));
        for info in nodes {
            if !self.info_hashes.get(&info_hash).map(|l| l.visited_ids.contains(&info.id)).unwrap_or(true) {
                self.info_hashes.get_mut(&info_hash).map(|l| l.visited_ids.insert(info.id));
                let other_prefix_length = info.id.kademlia(&info_hash)
                    .prefix_length();
                if other_prefix_length == 160 {
                    continue;
                }
                if other_prefix_length > prefix_length {
                    let _ = self.dht.query(&info.endpoint, Query::GetPeers {
                        info_hash
                    }).await;
                }
            }
        }
        Ok(())
    }

    async fn handle_event(&mut self, event: DHTEvent) -> Result<()> {
        use DHTEvent::*;
        match event {
            FoundNode(endpoint)  => {
                debug!("Received da DHT shitnode {endpoint}");
                let _ = self.dht.query(&endpoint, Query::FindNode {
                    target: self.id
                }).await;
            }
            InfoHashRequest(info_hash, tx) => {
                debug!("Received info hash request {info_hash}");
                self.info_hashes.insert(info_hash, InfoHashLookup::new(tx));
                if let Some(closest_node) = self.buckets
                    .iter().flat_map(|b| b.nodes.iter())
                    .min_by_key(|n| n.info.id.kademlia(info_hash))
                {
                    let _ = self.dht.query(&closest_node.info.endpoint, Query::GetPeers {
                        info_hash
                    }).await;
                }
            }
            _ => (),
        }
        Ok(())
    }
}
