use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, watch};
use tracing::debug;

use crate::libvictoria::{
    proto::{bit_torrent::Message, metadata::MetadataMessage},
    signal::Decay,
    types::*,
};

use super::control::{PeerProgress, Progress};

const INITIAL_TRANSFER_MESSAGES_CAPACITY: usize = 5;

const MAX_METADATA_REQUESTS: usize = 2;
const MIN_RECONNECTION_INTERVAL: Duration = Duration::from_secs(5);

pub enum PeerState {
    Disconnected {
        at: Instant,
        interval: Duration,
        reconnecting: bool,
        reason: Option<anyhow::Error>,
    },
    Connected {
        at: Instant,
        sent_metadata_requests: usize,
        tx: mpsc::Sender<Message>,
        initial_transfer_messages: VecDeque<Message>,
        received_extension_handshake: bool,
    },
}

impl PeerState {
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }

    pub fn disconnect(&mut self, reason: Option<anyhow::Error>) {
        *self = Self::Disconnected {
            at: Instant::now(),
            interval: MIN_RECONNECTION_INTERVAL,
            reconnecting: false,
            reason,
        }
    }

    pub fn connect(&mut self, tx: mpsc::Sender<Message>) {
        *self = Self::Connected {
            initial_transfer_messages: VecDeque::with_capacity(INITIAL_TRANSFER_MESSAGES_CAPACITY),
            at: Instant::now(),
            sent_metadata_requests: 0,
            received_extension_handshake: false,
            tx,
        }
    }

    pub fn back_off(&mut self) {
        if let Self::Disconnected {
            interval,
            reconnecting,
            at,
            ..
        } = self
        {
            *interval *= 2;
            *reconnecting = false;
            *at = Instant::now();
        }
    }
}

pub struct Peer {
    progress_tx: watch::Sender<Progress>,
    pub state: PeerState,
    info: PeerInfo,
    client: Option<String>,
    max_requests: Option<usize>,
    supports_fast: bool,
    supports_pex: bool,
    supports_metadata: bool,
    supports_dht: bool,
    discovery_mechanisms: HashSet<DiscoveryMechanism>,

    disconnects: Decay,

    downloaded_metadata_this_second: usize,
    uploaded_metadata_this_second: usize,

    last_download_rate: usize,
}

impl Peer {
    const DISCONNECTS_HALF_LIFE: Duration = Duration::from_mins(15);

    pub fn new(
        info: PeerInfo,
        tx: mpsc::Sender<Message>,
        progress_tx: watch::Sender<Progress>,
        supports_fast: bool,
        supports_dht: bool,
        discovery_mechanism: DiscoveryMechanism,
    ) -> Self {
        let peer = Self {
            client: None,
            info,
            progress_tx,
            supports_fast,
            supports_dht,
            max_requests: None,
            supports_metadata: false,
            supports_pex: false,
            uploaded_metadata_this_second: 0,
            downloaded_metadata_this_second: 0,
            disconnects: Decay::new(Self::DISCONNECTS_HALF_LIFE, None),
            state: PeerState::Disconnected {
                reconnecting: false,
                at: Instant::now(),
                interval: MIN_RECONNECTION_INTERVAL,
                reason: None,
            },
            discovery_mechanisms: HashSet::from([discovery_mechanism]),

            last_download_rate: 0,
        };
        peer.init_progress();
        peer
    }

    fn init_progress(&self) {
        if let Some(id) = self.info.id {
            self.progress_tx.send_modify(|p| {
                p.peers.insert(
                    id,
                    PeerProgress {
                        id,
                        discovery_mechanisms: self.discovery_mechanisms.clone(),
                        endpoints: self.info.endpoints.0.clone(),
                        connection: None,
                        client: self.client.clone(),
                        supports_dht: self.supports_dht,
                        supports_fast: self.supports_fast,
                        supports_metadata: self.supports_metadata,
                        supports_pex: self.supports_pex,
                        metadata_down_speed: self.downloaded_metadata_this_second,
                        metadata_up_speed: self.uploaded_metadata_this_second,
                        errors: Vec::new(),
                    },
                );
            });
        }
    }

    fn notify(&self, f: impl Fn(&mut PeerProgress)) {
        if let Some(id) = &self.info.id {
            self.progress_tx.send_modify(|p| {
                p.peers.get_mut(id).map(|c| f(c));
            });
        }
    }

    pub fn disconnects(&mut self) -> f64 {
        self.disconnects.value()
    }

    pub fn uploaded_metadata_this_second(&self) -> usize {
        self.uploaded_metadata_this_second
    }
    pub fn downloaded_metadata_this_second(&self) -> usize {
        self.downloaded_metadata_this_second
    }
    pub fn info(&self) -> &PeerInfo {
        &self.info
    }
    pub fn client(&self) -> Option<&String> {
        self.client.as_ref()
    }
    pub fn supports_fast(&self) -> bool {
        self.supports_fast
    }
    pub fn supports_pex(&self) -> bool {
        self.supports_pex
    }
    pub fn supports_metadata(&self) -> bool {
        self.supports_metadata
    }
    pub fn supports_dht(&self) -> bool {
        self.supports_dht
    }

    pub fn modify_info(&mut self, f: impl Fn(&mut PeerInfo)) {
        f(&mut self.info);
        self.notify(|p| p.endpoints.extend(self.info.endpoints.iter().cloned()));
    }
    pub fn add_discovery_mechanism(&mut self, mechanism: DiscoveryMechanism) {
        self.discovery_mechanisms.insert(mechanism);
        self.notify(|p| {
            p.discovery_mechanisms.insert(mechanism);
        });
    }

    pub fn reset_statistics(&mut self) {
        self.uploaded_metadata_this_second = 0;
        self.downloaded_metadata_this_second = 0;
        self.notify(|p| {
            p.metadata_down_speed = self.downloaded_metadata_this_second;
            p.metadata_up_speed = self.uploaded_metadata_this_second;
        });
    }

    pub fn can_request_metadata(&self) -> bool {
        if let PeerState::Connected {
            sent_metadata_requests,
            received_extension_handshake,
            ..
        } = &self.state
        {
            return *sent_metadata_requests < MAX_METADATA_REQUESTS
                && *received_extension_handshake;
        }
        false
    }

    pub fn sub_sent_metadata_requests(&mut self, count: usize) {
        if let PeerState::Connected {
            sent_metadata_requests,
            ..
        } = &mut self.state
        {
            *sent_metadata_requests = sent_metadata_requests.saturating_sub(count);
        }
    }

    pub fn disconnect(&mut self, reason: Option<anyhow::Error>) {
        self.disconnects.change(|d| d + 1.);
        if let Some(reason) = &reason {
            self.notify(|p| p.errors.push(format!("{reason:#}")));
        }
        self.state.disconnect(reason);
    }

    pub async fn send(&self, msg: Message) {
        if let PeerState::Connected { tx, .. } = &self.state {
            if let Err(_) = tx.send(msg).await {
                debug!("Tried to send message to a closed channel");
            }
        }
    }

    pub async fn request_metadata(&mut self, index: usize) {
        self.send(Message::Metadata(MetadataMessage::Request { index }))
            .await;
        if let PeerState::Connected {
            sent_metadata_requests,
            ..
        } = &mut self.state
        {
            *sent_metadata_requests += 1;
        }
    }
    pub async fn send_metadata(&mut self, index: usize, metadata_size: usize, piece: Vec<u8>) {
        self.uploaded_metadata_this_second += 1;
        self.send(Message::Metadata(MetadataMessage::Data {
            index,
            total_size: metadata_size,
            piece,
        }))
        .await;
    }
    pub async fn send_metadata_reject(&mut self, index: usize) {
        self.send(Message::Metadata(MetadataMessage::Reject { index }))
            .await;
    }

    pub fn reject_metadata(&mut self) {
        self.sub_sent_metadata_requests(1);
    }
    pub fn timeout_metadata(&mut self, count: usize) {
        self.sub_sent_metadata_requests(count);
    }
    pub fn receive_metadata(&mut self, downloading: bool) {
        self.downloaded_metadata_this_second += 1;
        if downloading {
            self.sub_sent_metadata_requests(1);
        }
    }

    pub fn queue_transfer_message(&mut self, message: Message) {
        if let PeerState::Connected {
            initial_transfer_messages,
            ..
        } = &mut self.state
        {
            initial_transfer_messages.push_back(message);
        }
    }

    pub fn receive_extension_handshake(
        &mut self,
        client: Option<String>,
        max_requests: Option<usize>,
        extensions: HashMap<String, u8>,
    ) {
        self.client = client;
        self.supports_metadata = extensions.contains_key("ut_metadata");
        self.supports_pex = extensions.contains_key("ut_pex");
        self.max_requests = max_requests;
        if let PeerState::Connected {
            received_extension_handshake,
            ..
        } = &mut self.state
        {
            *received_extension_handshake = true;
        }
        self.notify(|p| {
            p.supports_metadata = self.supports_metadata;
            p.supports_pex = self.supports_pex;
            p.client = self.client.clone();
        });
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(id) = &self.info.id {
            self.progress_tx.send_modify(|p| {
                p.peers.remove(id);
            });
        }
    }
}

fn is_disconnection_boring(error: anyhow::Error) -> bool {
    error.downcast_ref::<std::io::Error>().is_some_and(|e| {
        matches!(
            e.kind(),
            std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::BrokenPipe
        )
    })
}
