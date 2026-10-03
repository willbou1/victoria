mod piece;
mod peer;
mod transfer;
pub mod control;

use anyhow::{Result, anyhow};
use sha1::{Digest, Sha1};
use tokio::{
    fs,
    sync::mpsc,
    sync::watch,
    io::AsyncWriteExt,
};
use tracing::{info, warn, debug, Instrument, Span};
use transfer::Transfer;
use url::Url;
use std::{
    collections::HashMap, path::{PathBuf},
    time::Duration,
    path::Path,
};

use crate::{
    metainfo::{Metadata, Metainfo},
    proto::{bit_torrent::{BitTorrent, Message},
    metadata::MetadataMessage,
    pex::PEXMessage, tracker},
    tracker::{TrackerEvent, Trackers},
    types::*
};
use piece::Piece;
use peer::{Peer, PeerState};
use control::*;

const AUTOSAVE_INTERVAL: Duration = Duration::from_mins(2);

const METADATA_BLOCK_SIZE: usize = 16 * 1024;

const MAX_DISCOVERY_ATTEMPTS: usize = 3;

const PEER_CHANNEL_CAPACITY: usize = 100;
const EVENT_CHANNEL_CAPACITY: usize = 400;

#[derive(PartialEq, Eq)]
enum DiscoveryMechanism {
    Tracker,
    PEX,
    DHT,
}

struct DiscoveryAttempt {
    info: PeerInfo,
    num_attempts: usize,
    mechanism: DiscoveryMechanism,
}

impl DiscoveryAttempt {
    fn new(info: PeerInfo, mechanism: DiscoveryMechanism) -> Self {
        Self {
            info,
            mechanism,
            num_attempts: 1,
        }
    }
}

pub(crate) enum Event {
    Message(PeerId, Message),

    Connection(BitTorrent, PeerInfo, Option<PeerId>),
    ConnectionFailure(PeerInfo, anyhow::Error, Option<PeerId>),
    Disconnection(PeerId, anyhow::Error),

    Tracker(Vec<PeerInfo>),
}

pub struct Torrent {
    metainfo: Metainfo,

    peers: HashMap<PeerId, Peer>,
    rx: mpsc::Receiver<Event>,
    peer_tx: mpsc::Sender<Event>,
    tracker_tx: watch::Sender<tracker::Progress>,
    tracker_event_tx: mpsc::Sender<TrackerEvent>,
    info_hash: Hash,
    display_name: String,
    span: Span,
    discovery_attemps: Vec<DiscoveryAttempt>,

    transfer: Option<Transfer>,

    metadata: Piece,

    pub command_tx: mpsc::Sender<Command>,
    command_rx: mpsc::Receiver<Command>,
    pub progress_rx: watch::Receiver<Progress>,
    progress_tx: watch::Sender<Progress>,
    config_rx: watch::Receiver<Config>,
}

impl Torrent {
    async fn check_already_exists(config_rx: &watch::Receiver<Config>, info_hash: &Hash) -> Result<bool> {
        Ok(fs::try_exists(
            config_rx.borrow().data_path.join(info_hash.to_string())
        ).await?)
    }
    
    pub async fn from_magnet(url: &str, config_rx: watch::Receiver<Config>) -> Result<Option<Self>> {
        let url = Url::parse(url).unwrap();
        let mut pairs = url.query_pairs();
        let xt = pairs.find(|(n, _)| n == "xt").unwrap();
        let dn = pairs.find(|(n, _)| n == "dn").unwrap();
        let display_name = dn.1.into_owned();

        let info_hash = Hash::from_xt(&xt.1).unwrap();
        if Self::check_already_exists(&config_rx, &info_hash).await? {
            return Ok(None);
        }

        let trackers: Vec<_> = pairs.filter(|(n, _)| n == "tr")
            .map(|(_, v)| v.into_owned()).collect();

        let metadata_piece = Piece::new(None, METADATA_BLOCK_SIZE, 0, info_hash, false);

        Ok(Some(Self::new(
            &display_name,
            info_hash,
            config_rx,
            Metainfo::from_magnet(trackers),
            metadata_piece,
            None,
        ).await?))
    }

    pub async fn from_torrent_file(
        path: &PathBuf,
        config_rx: watch::Receiver<Config>,
    ) -> Result<Option<Self>> {
        let file = fs::read(path).await?;
        let (metainfo, metadata_bytes) = Metainfo::from_bytes(&file)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        {}
        info!("Parsed metainfo:\n{}", metainfo);

        let metadata_bytes = metadata_bytes.ok_or(anyhow!("No metadata in metainfo"))?;
        let metadata = Metadata::from_bytes(&metadata_bytes)
            .map_err(|e| anyhow::anyhow!(e))?;
        info!("Parsed metadata:\n{}", metadata);

        let info_hash = Hash::from(Sha1::digest(&metadata_bytes).into());
        if Self::check_already_exists(&config_rx, &info_hash).await? {
            return Ok(None);
        }

        let metadata_piece = Piece::from_slice(METADATA_BLOCK_SIZE, info_hash, &metadata_bytes);

        Ok(Some(Self::new(
            &metadata.name.clone(),
            info_hash,
            config_rx,
            metainfo,
            metadata_piece,
            Some(metadata),
        ).await?))
    }

    async fn find_torrent_file(dir: &Path) -> Result<PathBuf> {
        let mut entries = fs::read_dir(dir).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.is_file()
                && path.extension().is_some_and(|ext| ext == "torrent")
            {
                return Ok(path);
            }
        }

        Err(anyhow!("No .torrent file found"))
    }

    pub async fn from_info_hash(
        info_hash_str: &str,
        config_rx: watch::Receiver<Config>,
    ) -> Result<Self> {
        let dir_path = config_rx.borrow().data_path
            .join(info_hash_str);
        let path = Self::find_torrent_file(&dir_path).await?;
        let file = fs::read(&path).await?;
        let (metainfo, metadata_bytes) = Metainfo::from_bytes(&file)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        {}
        info!("Parsed metainfo:\n{}", metainfo);

        if let Some(metadata_bytes) = metadata_bytes {
            let metadata = Metadata::from_bytes(&metadata_bytes)
                .map_err(|e| anyhow::anyhow!(e))?;
            info!("Parsed metadata:\n{}", metadata);

            let info_hash = Hash::from(Sha1::digest(&metadata_bytes).into());
            let metadata_piece = Piece::from_slice(METADATA_BLOCK_SIZE, info_hash, &metadata_bytes);

            Self::new(
                &metadata.name.clone(),
                info_hash,
                config_rx,
                metainfo,
                metadata_piece,
                Some(metadata),
            ).await
        } else {
            let info_hash = Hash::from(
                hex::decode(info_hash_str)?
                    .try_into()
                    .map_err(|e: Vec<u8>| anyhow!("invalid info hash length: {}", e.len()))?
            );
            let metadata_piece = Piece::new(None, METADATA_BLOCK_SIZE, 0, info_hash, false);
            let display_name = path.file_stem()
                .ok_or(anyhow!("No file name to use as display name"))?.to_string_lossy();

            Self::new(
                &display_name,
                info_hash,
                config_rx,
                metainfo,
                metadata_piece,
                None,
            ).await
        }
    }

    async fn new(
        display_name: &str,
        info_hash: Hash,
        config_rx: watch::Receiver<Config>,
        metainfo: Metainfo,
        metadata_piece: Piece,
        metadata: Option<Metadata>,
    ) -> Result<Self> {
        let span = tracing::info_span!(
            "torrent",
            display_name = %display_name,
        );
        let _enter = span.enter();

        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);

        let (progress_tx, progress_rx) = watch::channel(Progress {
            display_name: display_name.to_string(),
            num_peers: 0,
            num_connected_peers: 0,
            num_discovery_attempts: 0,
            transfer: None,
            metadata_bitfield: metadata_piece.to_bitfield(),
            peers: HashMap::new(),
            trackers: HashMap::new(),
        });
        let (command_tx, command_rx) = mpsc::channel(10);

        let (tracker_event_tx, tracker_event_rx) = mpsc::channel(10);
        let (tracker_tx, tracker_rx) = watch::channel(tracker::Progress {
            downloaded: 0,
            uploaded: 0,
            left: 0,
            event: tracker::Event::None,
        });
        let trackers = Trackers::new(
            tx.clone(),
            tracker_rx,
            progress_tx.clone(),
            tracker_event_rx,
            config_rx.borrow().client_id,
            info_hash,
            metainfo.announces.clone(),
        );
        tokio::task::Builder::new()
            .name("trackers")
            .spawn(trackers.run().instrument(span.clone()))
            .unwrap();

        let transfer = if let Some(metadata) = metadata {
            Some(Transfer::new(
                metadata,
                info_hash,
                tracker_tx.clone(),
                progress_tx.clone(),
                config_rx.clone()
            ).await?)
        } else {
            None
        };

        Ok(Self {
            display_name: display_name.to_string(),
            peers: HashMap::new(),
            peer_tx: tx,
            tracker_tx,
            tracker_event_tx,
            metadata: metadata_piece,
            transfer,
            rx,
            span: span.clone(),
            discovery_attemps: Vec::new(),
            metainfo,
            info_hash,

            config_rx,
            progress_rx,
            progress_tx,
            command_rx,
            command_tx
        })
    }
    async fn write_metainfo(&self) -> Result<()> {
        let path = self.config_rx.borrow().data_path
            .join(self.info_hash.to_string())
            .join(format!("{}.torrent", self.display_name));
        fs::create_dir_all(&path.parent().unwrap()).await?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .open(&path).await?;
        file.write_all(
            &self.metainfo.to_bytes(self.transfer.as_ref().map(|t| &t.metadata))
        ).await?;
        Ok(())
    }

    pub async fn run(&mut self,) -> Result<()> {
        let span = self.span.clone();
        async {
            self.write_metainfo().await?;
            let mut tick_interval = tokio::time::interval(Duration::from_secs(1));
            let mut autosave_interval = tokio::time::interval(AUTOSAVE_INTERVAL);
            loop {
                tokio::select! {
                    event = self.rx.recv() => {
                        self.handle_event(event.unwrap()).await?;
                    }
                    _ = autosave_interval.tick() => {
                        if let Some(transfer) = &mut self.transfer {
                            transfer.save_state().await?;
                        }
                    }
                    _ = tick_interval.tick() => {
                        self.tick().await?;
                    }
                    command = self.command_rx.recv() => {
                        match command {
                            Some(Command::Stop) | None => {
                                if let Some(transfer) = &mut self.transfer {
                                    transfer.save_state().await?;
                                }
                                break;
                            }
                            Some(Command::Delete) => {
                                let path = self.config_rx.borrow().data_path
                                    .join(self.info_hash.to_string());
                                fs::remove_dir_all(path).await?;
                                break;
                            }
                            Some(command) => self.handle_command(command).await?,
                        }
                    }
                }
            }
            Ok(())
        }.instrument(span).await
    }

    async fn handle_command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::ReloadTrackers => {
                self.tracker_event_tx.send(TrackerEvent::Reload).await?;
            }
            _ => (),
        }
        Ok(())
    }

    fn statistics(&mut self) {
        const VERBOSE_DISCOVERY: bool = false;
        
        let mut status = String::new();

        status.push_str("    Discovery attemps:\n        Tracker: ");
        let tracker_attemps = self.discovery_attemps.iter()
            .filter(|a| a.mechanism == DiscoveryMechanism::Tracker);
        if VERBOSE_DISCOVERY {
            for attempt in tracker_attemps {
                status.push_str(
                    &format!("{} ({}) ", attempt.info, attempt.num_attempts));
            }
        } else {
            status.push_str(&format!("{}", tracker_attemps.count()));
        }

        status.push_str("\n        PEX: ");
        let pex_attemps = self.discovery_attemps.iter()
            .filter(|a| a.mechanism == DiscoveryMechanism::PEX);
        if VERBOSE_DISCOVERY {
            for attempt in pex_attemps {
                status.push_str(
                    &format!("{} ({}) ", attempt.info, attempt.num_attempts));
            }
        } else {
            status.push_str(&format!("{}", pex_attemps.count()));
        }

        for (id, peer) in self.peers.iter_mut() {
            self.progress_tx.send_modify(|prog| {
                prog.peers.entry(*id).and_modify(
                    |p| {
                        p.metadata_down_speed = peer.downloaded_metadata_this_second();
                        p.metadata_up_speed = peer.uploaded_metadata_this_second();
                    }
                );
            });
            peer.reset_statistics();
        }

        info!("\n{}", status);
    }

    async fn tick(&mut self) -> Result<()> {
        if self.transfer.is_none() {
            for (peer_id, count) in self.metadata.check_timeout() {
                self.peers.entry(peer_id)
                    .and_modify(|p| p.timeout_metadata(count));
            }
            self.dispatch_metadata_request().await?;
        }

        let mut reconnect = Vec::new();
        for peer in self.peers.values_mut() {
            if let PeerState::Disconnected {at, interval, reconnecting, ..} = &mut peer.state {
                if at.elapsed() > *interval && !*reconnecting {
                    *reconnecting = true;
                    reconnect.push(peer.info.clone());
                }
            }
        }
        for info in reconnect {
            self.try_connect(&info, info.id);
        }

        self.statistics();

        if let Some(transfer) = &mut self.transfer {
            transfer.tick().await?;
        }

        Ok(())
    }

    fn try_connect(&self, info: &PeerInfo, known_id: Option<PeerId>) {
        let tx = self.peer_tx.clone();
        let peer_info = info.clone();
        let info_hash = self.info_hash.clone();
        let client_id = self.config_rx.borrow().client_id;
        tokio::task::Builder::new()
            .name(if known_id.is_some() {
                "Reconnection"
            } else {
                "Discovery"
            })
            .spawn(async move {
                match BitTorrent::handshake(
                    &peer_info,
                    &info_hash,
                    &client_id,
                ).await {
                    Ok(bit_torrent) => tx.send(Event::Connection(bit_torrent, peer_info, known_id)).await,
                    Err(e) => tx.send(Event::ConnectionFailure(peer_info, e, known_id)).await,
                }
            }.instrument(self.span.clone()))
            .unwrap();
    }

    fn discover(&mut self, info: &PeerInfo, mechanism: DiscoveryMechanism) {
        if self.peers.iter().all(|(_, c)| !c.info.is_same_peer(info)) {
            match self.discovery_attemps.iter_mut().find(|a| a.info.is_same_peer(&info)) {
                Some(attempt) => {
                    if attempt.num_attempts < MAX_DISCOVERY_ATTEMPTS {
                        attempt.num_attempts += 1;
                        self.try_connect(info, None);
                    } else {
                        self.discovery_attemps.retain(|a| &a.info != info);
                    }
                },
                None => {
                    self.discovery_attemps.push(DiscoveryAttempt::new(info.clone(), mechanism));
                    self.try_connect(info, None);
                },
            }
        }
    }

    async fn dispatch_metadata_request(&mut self) -> Result<()> {
        for (peer_id, peer) in &mut self.peers {
            while peer.can_request_metadata() {
                if let Some(index) = self.metadata.find_available_block(&peer_id) {
                    let span = tracing::info_span!(
                        "peer",
                        id = %peer_id,
                    );
                    let _enter = span.enter();
                    debug!("Requested metadata {index}");
                    self.metadata.download(index, *peer_id);
                    peer.request_metadata(index).await;
                } else {
                    break;
                }
            }
        }
        Ok(())
    }

    async fn handle_metadata_message(&mut self, message: MetadataMessage, peer_id: &PeerId) -> Result<()> {
        match message {
            MetadataMessage::Request { index } => {
                debug!("Got metadata request {index}");
                if let Some(peer) = self.peers.get_mut(peer_id) && self.metadata.num_blocks() != 0 {
                    if let Some(piece) = self.metadata.get(index) {
                        debug!("Sent metadata {index}");
                        peer.send_metadata(index, self.metadata.num_blocks(), piece.to_vec()).await;
                    } else {
                        debug!("Rejected metadata {index}");
                        peer.send_metadata_reject(index).await;
                    }
                }
            },
            MetadataMessage::Data { index, piece, .. } => {
                if self.transfer.is_none() && !self.metadata.has_obtrined(index) {
                    debug!("Got metadata block {index}");
                    let who_downloading = self.metadata.who_downloading(index);
                    self.peers.entry(*peer_id).and_modify(
                        |p| p.receive_metadata(who_downloading.map(|w| w == peer_id).unwrap_or(false))
                    );
                    if let Some(who) = who_downloading && who != peer_id {
                        self.peers.get_mut(who).map(|p| p.sub_sent_metadata_requests(1));
                    }

                    if let Some(metadata_bytes) = self.metadata.place(index, piece, false) {
                        let metadata = Metadata::from_bytes(&metadata_bytes)
                            .map_err(|e| anyhow::anyhow!("Error parsing metadata: {e}"))?;
                        info!("Got metadata:\n{metadata}");
                        let mut transfer = Transfer::new(
                            metadata,
                            self.info_hash,
                            self.tracker_tx.clone(),
                            self.progress_tx.clone(),
                            self.config_rx.clone(),
                        ).await?;
                        for (peer_id, peer) in self.peers.iter_mut() {
                            if let PeerState::Connected { tx, initial_transfer_messages, .. } = &mut peer.state {
                                transfer.add_connection(
                                    *peer_id,
                                    tx.clone(),
                                    peer.supports_fast,
                                    Some(initial_transfer_messages),
                                ).await?;
                            }
                        }
                        self.transfer = Some(transfer);
                        self.write_metainfo().await?;
                    } else {
                        self.dispatch_metadata_request().await?;
                    }
                    self.progress_tx.send_modify(|p| {
                        p.metadata_bitfield = self.metadata.to_bitfield();
                    });
                }
            },
            MetadataMessage::Reject { index } => {
                if self.transfer.is_none() {
                    let who_downloading = self.metadata.who_downloading(index);
                    if let Some(who) = who_downloading && who == peer_id {
                        self.metadata.reject(index, *peer_id);
                        self.peers.entry(*peer_id)
                            .and_modify(|p| p.reject_metadata());
                        self.dispatch_metadata_request().await?;
                    }
                }
                debug!("Got metadata reject {index}");
            },
            _ => (),
        }
        Ok(())
    }

    fn update_progress(&self) {
        self.progress_tx.send_modify(|p| {
            p.num_peers = self.peers.len();
            for (id, peer) in &self.peers {
                let peer_progress = p.peers.entry(*id).or_insert(PeerProgress::new(*id));
                peer_progress.endpoints.extend(peer.info.endpoints.iter().cloned());
                peer_progress.client = peer.client.clone();
                peer_progress.supports_dht = peer.supports_dht;
                peer_progress.supports_fast = peer.supports_fast;
                peer_progress.supports_metadata = peer.supports_metadata;
                peer_progress.supports_pex = peer.supports_pex;
                peer_progress.metadata_down_speed = peer.downloaded_metadata_this_second();
                peer_progress.metadata_up_speed = peer.uploaded_metadata_this_second();
            }
            p.num_connected_peers = self.peers.iter()
                .filter(|p| p.1.state.is_connected()).count();
            p.num_discovery_attempts = self.discovery_attemps.len();
        });
    }

    async fn handle_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Message(peer_id, message) => {
                if let Some(peer) = self.peers.get_mut(&peer_id) {
                    let span = tracing::info_span!(
                        "peer",
                        id = %peer_id,
                    );
                    let guard = span.enter();
                    // dispatch to transfer layer
                    match message {
                        Message::Metadata(msg) => self.handle_metadata_message(msg, &peer_id).await?,

                        Message::KeepAlive => {}

                        Message::ExtensionHandshake { extensions, client, max_requests, metadata_size } => {
                            debug!("Got extension handshake {extensions:?} {client:?} {max_requests:?}");
                            peer.receive_extension_handshake(client, extensions);
                            if let Some(metadata_size) = metadata_size && self.transfer.is_none() && metadata_size / (16 * 1024) > self.metadata.num_blocks() {
                                warn!("Resetting metadata");
                                self.metadata.set_length_and_reset(metadata_size);
                                drop(guard);
                                self.dispatch_metadata_request().await?;
                            }
                        }

                        Message::PEX(PEXMessage { added, dropped }) => {
                            debug!("Got PEX {added:?} {dropped:?}");
                            for info in added {
                                self.discover(&info, DiscoveryMechanism::PEX);
                            }
                        }

                        Message::DHTPort { port } => {
                            debug!("DHT port {port}");
                        }

                        Message::Unsupported { type_byte } => 
                            warn!("Got unsupported message {type_byte}"),
                        Message::UnsupportedExtension { type_byte } =>
                            warn!("Got unsupported extension message {type_byte}"),

                        msg @ _ => {
                            if let Some(transfer) = &mut self.transfer {
                                transfer.handle_event(&peer_id, msg).await?;
                            } else {
                                self.peers.get_mut(&peer_id)
                                    .map(|p| p.queue_transfer_message(msg));
                            }
                        }
                    }
                }
            }

            Event::Connection(bit_torrent, mut info, known_id) => {
                let (tx, rx) = mpsc::channel(PEER_CHANNEL_CAPACITY);

                if let Some(id) = known_id {
                    debug!(id = %id,"Reconnected");
                    self.peers.entry(id)
                        .and_modify(|p| p.state.reconnect(tx.clone()));
                } else {
                    self.discovery_attemps.retain(|a| a.info != info);
                    info.id = Some(bit_torrent.id);
                    debug!(peer = %info,"Discovered");
                    if let Some((_, m)) = self.peers.iter_mut().find(|(_, c)| c.info.is_same_peer(&info)) {
                        m.info.merge(info.clone());
                    } else {
                        self.peers.insert(
                            info.id.unwrap(),
                            Peer::new(info.clone(), tx.clone(), bit_torrent.supports_fast, bit_torrent.supports_dht),
                        );
                    }
                }

                if let Some(transfer) = &mut self.transfer {
                    transfer.add_connection(
                        info.id.unwrap(),
                        tx,
                        bit_torrent.supports_fast,
                        None,
                    ).await?;
                }
                tokio::task::Builder::new()
                    .name("BitTorrent")
                    .spawn(
                        bit_torrent.run(self.peer_tx.clone(), rx).instrument(self.span.clone())
                    )
                    .unwrap();
            }

            Event::ConnectionFailure(peer_info, e, known_id) => {
                self.discover(&peer_info, DiscoveryMechanism::Tracker);
                if let Some(id) = known_id {
                    debug!(peer = %peer_info, "Couldn't reconnect ({e})");
                    self.peers.get_mut(&id).unwrap().state.back_off();
                } else {
                    debug!(peer = %peer_info, "Couldn't discover ({e})");
                }
            }

            Event::Disconnection(peer_id, error) => {
                if let Some(transfer) = &mut self.transfer {
                    transfer.sever_connection(&peer_id);
                }
                self.peers.entry(peer_id).and_modify(|p| {
                    p.state.disconnect(error);
                    self.progress_tx.send_modify(|progress| {
                        let peer_progress = progress.peers.entry(peer_id).or_insert(PeerProgress::new(peer_id));
                        if let PeerState::Disconnected { reason, .. } = &p.state {
                            peer_progress.errors.push(format!("{reason:#}"));
                        }
                    });
                });
            }

            Event::Tracker(peer_infos) => {
                debug!("Tracker discovery");
                for info in peer_infos {
                    self.discover(&info, DiscoveryMechanism::Tracker);
                }
            }
        }

        self.update_progress();

        Ok(())
    }
}
