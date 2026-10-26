use anyhow::Result;
use tokio::{
    sync::mpsc,
    select,
};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, info_span, debug};
use std::{
    array,
    time::{Duration, Instant},
    collections::HashSet,
};

use super::{
    proto::dht::*,
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

pub enum DHTEvent {
    FoundNode(Endpoint),
}

pub struct DHTManager {
    id: NodeId,
    dht: DHT,
    buckets: [Bucket; 160],
    rx: mpsc::Receiver<DHTEvent>,
    pub tx: mpsc::Sender<DHTEvent>,
    cancellation_token: CancellationToken,

    discovered_ids: HashSet<NodeId>,
}

impl DHTManager {
    pub async fn new(cancellation_token: CancellationToken) -> Result<Self> {
        let (tx, rx) = mpsc::channel(100);
        let node_id = NodeId::random();
        Ok(Self {
            dht: DHT::bind(node_id, 6767).await?,
            id: node_id,
            buckets: array::from_fn(|_| Bucket::new()),
            tx,
            rx,
            cancellation_token,
            
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
                        Ok(message) => {
                            match message.type_of {
                                MessageType::Response {response, id} => match &response {
                                    Response::FindNode { nodes } => {
                                        self.find_nodes(&id, nodes).await?;
                                    }
                                    _ => (),
                                }
                                _ => (),
                            }
                        }
                        Err(_) => (),
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

    pub fn add_node(&mut self, prefix_length: u8, info: NodeInfo) {
        let bucket = &mut self.buckets[prefix_length as usize];
        if !bucket.is_full() {
            bucket.nodes.push(Node::new(info));
        }
    }

    async fn find_nodes(&mut self, id: &NodeId, nodes: &[NodeInfo]) -> Result<()> {
        let prefix_length = id.kademlia(&self.id).prefix_length() - 1;
        if !self.discovered_ids.contains(id) {
            self.add_node(prefix_length, NodeInfo {
                id: *id,
                endpoint: Endpoint::from_str("0.0.0.0", 6769),
            });
        } else {
            return Ok(());
        }
        for info in nodes {
            if !self.discovered_ids.contains(&info.id) {
                self.discovered_ids.insert(info.id);
                let other_prefix_length = info.id.kademlia(&self.id)
                    .prefix_length() - 1;
                if other_prefix_length < prefix_length {
                    self.dht.query(&info.endpoint, Query::FindNode {
                        target: self.id
                    }).await?;
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
                self.dht.query(&endpoint, Query::FindNode {
                    target: self.id
                }).await?;
            }
            _ => (),
        }
        Ok(())
    }
}
