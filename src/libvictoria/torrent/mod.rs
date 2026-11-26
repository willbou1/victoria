pub mod control;
mod peer;
mod piece;
mod transfer;

use anyhow::{Result, anyhow};
use rand::seq::SliceRandom;
use sha1::{Digest, Sha1};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{fs, io::AsyncWriteExt, select, sync::mpsc, sync::watch};
use tracing::{Instrument, Span, debug, info, info_span, trace, warn};
use transfer::Transfer;
use url::Url;

use crate::libvictoria::{
    dht::DHTEvent,
    metainfo::{Metadata, Metainfo},
    proto::{
        bit_torrent::{BitTorrent, Message},
        metadata::MetadataMessage,
        pex::PEXMessage,
        tracker,
    },
    tracker::{TrackerEvent, Trackers},
    types::*,
};
use control::*;
use peer::{Peer, PeerState};
use piece::Piece;

const METADATA_BLOCK_SIZE: usize = 16 * 1024;

const MAX_DISCOVERY_ATTEMPTS: usize = 3;

#[derive(Debug)]
struct Discovery {
    info: PeerInfo,
    num_attempts: usize,
    mechanisms: HashSet<DiscoveryMechanism>,
}

impl Discovery {
    fn new(info: PeerInfo, mechanism: DiscoveryMechanism) -> Self {
        Self {
            info,
            mechanisms: HashSet::from([mechanism]),
            num_attempts: 1,
        }
    }
}

pub(crate) enum ConnectionEvent {
    Connected {
        bit_torrent: BitTorrent,
        peer_info: PeerInfo,
        known_id: Option<PeerId>,
    },
    ConnectionFailed(PeerInfo, anyhow::Error, Option<PeerId>),
    Disconnected(PeerId, Option<anyhow::Error>),
}

pub(crate) enum CommandEvent {
    Stop,
    Pause,
    Resume,
    Delete,
    ToggleSequential,
    SetPriority(usize, FilePriority),
    Reload,
}

pub(crate) enum Event {
    Message(PeerId, Message),

    Tracker(Vec<PeerInfo>),
    DHT(HashSet<SocketAddr>),

    Connection(ConnectionEvent),
    Command(CommandEvent),
}

pub struct Torrent {
    metainfo: Metainfo,

    peers: HashMap<PeerId, Peer>,
    rx: mpsc::Receiver<Event>,
    pub peer_tx: mpsc::Sender<Event>,
    dht_tx: mpsc::Sender<DHTEvent>,
    tracker_progress_tx: watch::Sender<tracker::Progress>,
    tracker_event_tx: mpsc::Sender<TrackerEvent>,
    pub info_hash: Hash,
    display_name: String,
    span: Span,
    discoveries: Vec<Discovery>,
    queued_discoveries: VecDeque<Discovery>,

    transfer: Option<Transfer>,

    metadata: Piece,

    pub progress_rx: watch::Receiver<Progress>,
    progress_tx: watch::Sender<Progress>,
    config_rx: watch::Receiver<Config>,
}

impl Torrent {
    const PEER_CHANNEL_CAPACITY: usize = 100;
    const EVENT_CHANNEL_CAPACITY: usize = 400;

    const AUTOSAVE_INTERVAL: Duration = Duration::from_mins(2);
    const DHT_INTERVAL: Duration = Duration::from_mins(5);

    async fn check_already_exists(
        config_rx: &watch::Receiver<Config>,
        info_hash: &Hash,
    ) -> Result<bool> {
        Ok(fs::try_exists(config_rx.borrow().data_path.join(info_hash.to_string())).await?)
    }

    pub async fn from_magnet(
        url: &str,
        config_rx: watch::Receiver<Config>,
        dht_tx: mpsc::Sender<DHTEvent>,
    ) -> Result<Option<Self>> {
        let url = Url::parse(url)?;
        let mut pairs = url.query_pairs();
        let xt = pairs
            .find(|(n, _)| n == "xt")
            .ok_or(anyhow!("magnet link has no trackers"))?;
        let dn = pairs
            .find(|(n, _)| n == "dn")
            .ok_or(anyhow!("magnet link has no display name"))?;
        let display_name = dn.1.into_owned();

        let info_hash = Hash::from_xt(&xt.1)?;
        if Self::check_already_exists(&config_rx, &info_hash).await? {
            return Ok(None);
        }

        let trackers: Vec<_> = pairs
            .filter(|(n, _)| n == "tr")
            .map(|(_, v)| v.into_owned())
            .collect();

        let metadata_piece = Piece::new(None, METADATA_BLOCK_SIZE, 0, info_hash, false);

        Ok(Some(
            Self::new(
                display_name,
                info_hash,
                config_rx,
                dht_tx,
                Metainfo::from_magnet(trackers),
                metadata_piece,
                None,
            )
            .await?,
        ))
    }

    pub async fn from_torrent_file(
        path: &PathBuf,
        config_rx: watch::Receiver<Config>,
        dht_tx: mpsc::Sender<DHTEvent>,
    ) -> Result<Option<Self>> {
        let file = fs::read(path).await?;
        let (metainfo, metadata_bytes) = Metainfo::from_bytes(&file)?;
        info!("Parsed metainfo");

        let metadata_bytes = metadata_bytes.ok_or(anyhow!("No metadata in metainfo"))?;
        let metadata = Metadata::from_bytes(&metadata_bytes)?;
        info!("Parsed metadata");

        let info_hash = Hash(Sha1::digest(&metadata_bytes).into());
        if Self::check_already_exists(&config_rx, &info_hash).await? {
            return Ok(None);
        }

        let metadata_piece =
            Piece::from_slice(METADATA_BLOCK_SIZE, info_hash, &metadata_bytes, None);

        Ok(Some(
            Self::new(
                metadata.name.clone(),
                info_hash,
                config_rx,
                dht_tx,
                metainfo,
                metadata_piece,
                Some(metadata),
            )
            .await?,
        ))
    }

    async fn find_torrent_file(dir: &Path) -> Result<PathBuf> {
        let mut entries = fs::read_dir(dir).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|ext| ext == "torrent") {
                return Ok(path);
            }
        }

        Err(anyhow!("No .torrent file found"))
    }

    pub async fn from_info_hash(
        info_hash_str: &str,
        config_rx: watch::Receiver<Config>,
        dht_tx: mpsc::Sender<DHTEvent>,
    ) -> Result<Self> {
        let dir_path = config_rx.borrow().data_path.join(info_hash_str);
        let path = Self::find_torrent_file(&dir_path).await?;
        let file = fs::read(&path).await?;
        let (metainfo, metadata_bytes) = Metainfo::from_bytes(&file).map_err(|e| anyhow!("{e}"))?;
        {}
        info!("Parsed metainfo");

        if let Some(metadata_bytes) = metadata_bytes {
            let metadata = Metadata::from_bytes(&metadata_bytes).map_err(|e| anyhow!(e))?;
            info!("Parsed metadata");

            let info_hash = Hash(Sha1::digest(&metadata_bytes).into());
            let metadata_piece =
                Piece::from_slice(METADATA_BLOCK_SIZE, info_hash, &metadata_bytes, None);

            Self::new(
                metadata.name.clone(),
                info_hash,
                config_rx,
                dht_tx,
                metainfo,
                metadata_piece,
                Some(metadata),
            )
            .await
        } else {
            let info_hash = Hash(
                hex::decode(info_hash_str)?
                    .try_into()
                    .map_err(|e: Vec<u8>| anyhow!("invalid info hash length: {}", e.len()))?,
            );
            let metadata_piece = Piece::new(None, METADATA_BLOCK_SIZE, 0, info_hash, false);
            let display_name = path
                .file_stem()
                .ok_or(anyhow!("No file name to use as display name"))?
                .to_string_lossy()
                .to_string();

            Self::new(
                display_name,
                info_hash,
                config_rx,
                dht_tx,
                metainfo,
                metadata_piece,
                None,
            )
            .await
        }
    }

    async fn new(
        display_name: String,
        info_hash: Hash,
        config_rx: watch::Receiver<Config>,
        dht_tx: mpsc::Sender<DHTEvent>,
        metainfo: Metainfo,
        metadata_piece: Piece,
        metadata: Option<Metadata>,
    ) -> Result<Self> {
        let span = info_span!("torrent", display_name = %display_name);
        let _enter = span.enter();

        let (tx, rx) = mpsc::channel(Self::EVENT_CHANNEL_CAPACITY);

        let (progress_tx, progress_rx) = watch::channel(Progress::new(
            display_name.clone(),
            info_hash.clone(),
            metadata_piece.to_bitfield(),
        ));

        let (tracker_event_tx, tracker_event_rx) = mpsc::channel(10);
        let (tracker_progress_tx, tracker_progress_rx) = watch::channel(tracker::Progress {
            downloaded: 0,
            uploaded: 0,
            left: 0,
        });
        let trackers = Trackers::new(
            tx.clone(),
            tracker_progress_rx,
            progress_tx.clone(),
            tracker_event_rx,
            config_rx.clone(),
            info_hash,
            metainfo.announces.clone(),
        );
        tokio::task::Builder::new()
            .name("trackers")
            .spawn(trackers.run().instrument(span.clone()))?;

        let transfer = if let Some(metadata) = metadata {
            Some(
                Transfer::new(
                    metadata,
                    info_hash,
                    tracker_progress_tx.clone(),
                    tracker_event_tx.clone(),
                    progress_tx.clone(),
                    config_rx.clone(),
                )
                .await?,
            )
        } else {
            None
        };

        let discovery_cap = config_rx.borrow().discovery_cap;

        Ok(Self {
            display_name,
            peers: HashMap::new(),
            peer_tx: tx,
            tracker_progress_tx,
            dht_tx,
            tracker_event_tx,
            metadata: metadata_piece,
            transfer,
            rx,
            span: span.clone(),
            discoveries: Vec::with_capacity(discovery_cap),
            queued_discoveries: VecDeque::new(),
            metainfo,
            info_hash,

            config_rx,
            progress_rx,
            progress_tx,
        })
    }
    async fn write_metainfo(&self) -> Result<()> {
        let dir = self
            .config_rx
            .borrow()
            .data_path
            .join(self.info_hash.to_string());
        fs::create_dir_all(&dir).await?;
        let mut entries = fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|ext| ext == "torrent") {
                fs::remove_file(path).await?;
            }
        }
        let path = dir.join(format!("{}.torrent", self.display_name));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .open(&path)
            .await?;
        file.write_all(
            &self
                .metainfo
                .to_bytes(self.transfer.as_ref().map(|t| t.get_metadata())),
        )
        .await?;
        Ok(())
    }

    pub async fn run(&mut self) -> Result<()> {
        let span = self.span.clone();
        async {
            self.write_metainfo().await?;
            let mut tick_interval = tokio::time::interval(Duration::from_secs(1));
            let mut autosave_interval = tokio::time::interval(Self::AUTOSAVE_INTERVAL);
            let mut dht_interval = tokio::time::interval(Self::DHT_INTERVAL);
            loop {
                select! {
                    event = self.rx.recv() => {
                        match event {
                            Some(Event::Command(CommandEvent::Stop)) | None => {
                                if let Some(transfer) = &mut self.transfer {
                                    transfer.save_state().await?;
                                }
                                break;
                            }
                            Some(Event::Command(CommandEvent::Delete)) => {
                                let path = self.config_rx.borrow().data_path
                                    .join(self.info_hash.to_string());
                                fs::remove_dir_all(path).await?;
                                break;
                            }
                            Some(event) => self.handle_event(event).await?,
                        }
                    },
                    _ = autosave_interval.tick() => {
                        if let Some(transfer) = &mut self.transfer {
                            transfer.save_state().await?;
                        }
                    },
                    _ = dht_interval.tick() => {
                        let _ = self.dht_tx.send(
                            DHTEvent::InfoHashRequest(self.info_hash, self.peer_tx.clone())
                        ).await;
                    },
                    _ = tick_interval.tick() => self.tick().await?,
                }
            }
            Ok(())
        }
        .instrument(span)
        .await
    }

    fn statistics(&mut self) {
        for (id, peer) in self.peers.iter_mut() {
            self.progress_tx.send_modify(|prog| {
                prog.peers.entry(*id).and_modify(|p| {
                    p.metadata_down_speed = peer.downloaded_metadata_this_second();
                    p.metadata_up_speed = peer.uploaded_metadata_this_second();
                });
            });
            peer.reset_statistics();
        }
    }

    async fn tick(&mut self) -> Result<()> {
        if self.transfer.is_none() {
            for (peer_id, count) in self.metadata.check_timeout() {
                self.peers
                    .entry(peer_id)
                    .and_modify(|p| p.timeout_metadata(count));
            }
            self.dispatch_metadata_request().await;
        }

        self.statistics();

        if let Some(transfer) = &mut self.transfer {
            transfer.tick().await?;
        }

        self.evict_connections();

        Ok(())
    }

    fn evict_connections(&mut self) {
        let full = self.available_connection_slots() == 0;
        if let Some(transfer) = &mut self.transfer {
            for (id, peer) in &mut self.peers {
                if transfer.is_connection_useless(id).unwrap_or(false) {
                    warn!("Evicting cuz useless");
                    peer.disconnect(transfer.connection_download_rate(id).unwrap_or(0.), None);
                    transfer.sever_connection(id);
                    continue;
                }

                if let PeerState::Connected { at, .. } = peer.state
                    && full
                    && !transfer.completed()
                    && transfer
                        .connection_download_rate(id)
                        .map(|r| r < 16. * 1024.)
                        .unwrap_or(false)
                    && at.elapsed() > Duration::from_secs(15)
                {
                    warn!("Evicting cuz slow");
                    peer.disconnect(transfer.connection_download_rate(id).unwrap_or(0.), None);
                    transfer.sever_connection(id);
                }
            }
        }
    }

    fn furnish_connections(&mut self) -> Result<()> {
        if self.should_connect_to_peer() {
            let available = self.available_connection_slots();
            let mut reconnect = Vec::new();
            for (id, peer) in self.peers.iter_mut() {
                let download_rate = self
                    .transfer
                    .as_ref()
                    .and_then(|t| t.connection_download_rate(id))
                    .unwrap_or(peer.last_download_rate());
                if let PeerState::Disconnected {
                    at,
                    interval,
                    reconnecting,
                    ..
                } = &mut peer.state
                {
                    if at.elapsed() > *interval && !*reconnecting {
                        *reconnecting = true;
                        reconnect.push((peer.disconnects(), download_rate, peer.info().clone()));
                    }
                }
            }
            reconnect.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.total_cmp(&b.1)));
            for (disconnects, _, info) in reconnect.iter().take(available) {
                if *disconnects < 10. {
                    warn!("Trying out new connection");
                    self.try_connect(&info, info.id)?;
                }
            }
        }
        Ok(())
    }

    fn try_connect(&self, info: &PeerInfo, known_id: Option<PeerId>) -> Result<()> {
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
            .spawn(
                async move {
                    match BitTorrent::send_handshake(&peer_info, &info_hash, &client_id).await {
                        Ok(bit_torrent) => {
                            tx.send(Event::Connection(ConnectionEvent::Connected {
                                bit_torrent,
                                peer_info,
                                known_id,
                            }))
                            .await
                        }
                        Err(e) => {
                            tx.send(Event::Connection(ConnectionEvent::ConnectionFailed(
                                peer_info, e, known_id,
                            )))
                            .await
                        }
                    }
                }
                .instrument(self.span.clone()),
            )?;
        Ok(())
    }

    fn furnish_discoveries(&mut self) -> Result<()> {
        let discovery_cap = self.config_rx.borrow().discovery_cap;
        let num = discovery_cap
            .saturating_sub(self.discoveries.len())
            .min(self.queued_discoveries.len());
        for _ in 0..num {
            let Some(queued) = self.queued_discoveries.pop_front() else {
                break;
            };
            self.try_connect(&queued.info, None)?;
            self.discoveries.push(queued);
        }
        Ok(())
    }

    fn discover(&mut self, info: &PeerInfo, mechanism: Option<DiscoveryMechanism>) -> Result<()> {
        match self
            .peers
            .iter_mut()
            .find(|(_, c)| c.info().is_same_peer(info))
        {
            Some((_, peer)) => {
                if let Some(mechanism) = mechanism {
                    peer.add_discovery_mechanism(mechanism);
                }
            }
            None => {
                let idx = self
                    .discoveries
                    .iter()
                    .position(|a| a.info.is_same_peer(info));
                if let Some(idx) = idx {
                    if let Some(mechanism) = mechanism {
                        // Not a retry so let's just add the mechanism
                        self.discoveries[idx].mechanisms.insert(mechanism);
                    } else {
                        // This is only valid if the discovery is truly a retry... ask how I know SMH
                        if self.discoveries[idx].num_attempts < MAX_DISCOVERY_ATTEMPTS {
                            self.discoveries[idx].num_attempts += 1;
                            self.try_connect(info, None)?;
                        } else {
                            self.discoveries.remove(idx);
                            self.furnish_discoveries()?;
                        }
                    }
                } else if let Some(mechanism) = mechanism
                    && idx.is_none()
                    && self
                        .queued_discoveries
                        .iter()
                        .all(|q| !q.info.is_same_peer(info))
                {
                    if let Some(queued) = self
                        .queued_discoveries
                        .iter_mut()
                        .find(|q| q.info.is_same_peer(info))
                    {
                        // Already queued so let's just add the mechanism
                        queued.mechanisms.insert(mechanism);
                    } else {
                        let discovery_cap = self.config_rx.borrow().discovery_cap;
                        if self.discoveries.len() < discovery_cap {
                            self.discoveries
                                .push(Discovery::new(info.clone(), mechanism));
                            self.try_connect(info, None)?;
                        } else {
                            self.queued_discoveries
                                .push_back(Discovery::new(info.clone(), mechanism));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    async fn dispatch_metadata_request(&mut self) {
        let mut peer_ids: Vec<_> = self.peers.keys().copied().collect();
        loop {
            peer_ids.shuffle(&mut rand::rng());
            let mut requested = false;
            for peer_id in &peer_ids {
                let peer = self.peers.get_mut(peer_id).unwrap();
                if !peer.can_request_metadata() {
                    continue;
                }
                let Some(index) = self.metadata.find_available_block(peer_id) else {
                    return;
                };
                let span = info_span!("peer", id = %peer_id);
                let _enter = span.enter();
                debug!("Requested metadata {index}");
                self.metadata.download(index, *peer_id);
                peer.request_metadata(index).await;
                requested = true;
            }
            if !requested {
                break;
            }
        }
    }

    async fn handle_metadata_message(
        &mut self,
        message: MetadataMessage,
        peer_id: &PeerId,
    ) -> Result<()> {
        match message {
            MetadataMessage::Request { index } => {
                debug!("Got metadata request {index}");
                if let Some(peer) = self.peers.get_mut(peer_id)
                    && self.metadata.num_blocks() != 0
                {
                    if let Some(piece) = self.metadata.get(index) {
                        debug!("Sent metadata {index}");
                        peer.send_metadata(index, self.metadata.num_blocks(), piece.to_vec())
                            .await;
                    } else {
                        debug!("Rejected metadata {index}");
                        peer.send_metadata_reject(index).await;
                    }
                }
            }
            MetadataMessage::Data { index, piece, .. } => {
                if self.transfer.is_none() && !self.metadata.has_obtrined(index) {
                    debug!("Got metadata block {index}");
                    let who_downloading = self.metadata.who_downloading(index);
                    self.peers.entry(*peer_id).and_modify(|p| {
                        p.receive_metadata(who_downloading.map(|w| w == peer_id).unwrap_or(false))
                    });
                    if let Some(who) = who_downloading
                        && who != peer_id
                    {
                        self.peers
                            .get_mut(who)
                            .map(|p| p.sub_sent_metadata_requests(1));
                    }

                    if let Some(metadata_bytes) = self.metadata.place(index, piece, false) {
                        trace!(
                            "Got metadata bytes:\n{}",
                            String::from_utf8_lossy(&metadata_bytes)
                        );
                        let metadata = Metadata::from_bytes(&metadata_bytes)?;
                        info!("Parsed metadata from extension");

                        let mut transfer = Transfer::new(
                            metadata,
                            self.info_hash,
                            self.tracker_progress_tx.clone(),
                            self.tracker_event_tx.clone(),
                            self.progress_tx.clone(),
                            self.config_rx.clone(),
                        )
                        .await?;
                        for (peer_id, peer) in self.peers.iter_mut() {
                            let supports_fast = peer.supports_fast();
                            if let PeerState::Connected {
                                tx,
                                initial_transfer_messages,
                                ..
                            } = &mut peer.state
                            {
                                transfer
                                    .add_connection(
                                        *peer_id,
                                        tx.clone(),
                                        supports_fast,
                                        Some(initial_transfer_messages),
                                    )
                                    .await?;
                            }
                        }
                        self.transfer = Some(transfer);
                        self.write_metainfo().await?;
                    } else {
                        self.dispatch_metadata_request().await;
                    }
                    self.progress_tx.send_modify(|p| {
                        p.metadata_bitfield = self.metadata.to_bitfield();
                    });
                }
            }
            MetadataMessage::Reject { index } => {
                if self.transfer.is_none() {
                    let who_downloading = self.metadata.who_downloading(index);
                    if let Some(who) = who_downloading
                        && who == peer_id
                    {
                        self.metadata.reject(index, *peer_id);
                        self.peers
                            .entry(*peer_id)
                            .and_modify(|p| p.reject_metadata());
                        self.dispatch_metadata_request().await;
                    }
                }
                debug!("Got metadata reject {index}");
            }
            _ => (),
        }
        Ok(())
    }

    fn update_progress(&self) {
        self.progress_tx.send_modify(|p| {
            p.num_peers = self.peers.len();
            p.num_connected_peers = self
                .peers
                .iter()
                .filter(|p| p.1.state.is_connected())
                .count();
            p.num_discovery_attempts = self.discoveries.len();
        });
    }

    fn available_connection_slots(&self) -> usize {
        let connection_cap = self.config_rx.borrow().connection_cap;
        connection_cap.saturating_sub(
            self.peers
                .values()
                .filter(|p| p.state.is_connected())
                .count(),
        )
    }

    fn should_connect_to_peer(&self) -> bool {
        self.available_connection_slots() != 0
            && !self.transfer.as_ref().map_or(false, |t| t.completed())
    }

    async fn handle_connected(
        &mut self,
        bit_torrent: BitTorrent,
        mut peer_info: PeerInfo,
        known_id: Option<PeerId>,
    ) -> Result<()> {
        let (tx, rx) = mpsc::channel(Self::PEER_CHANNEL_CAPACITY);

        if let Some(id) = known_id {
            debug!(id = %id,"Reconnected");
            self.peers
                .entry(id)
                .and_modify(|p| p.state.connect(tx.clone()));
        } else {
            let removed = self
                .discoveries
                .extract_if(.., |a| a.info.is_same_peer(&peer_info))
                .next();
            self.furnish_discoveries()?;
            peer_info.id = Some(bit_torrent.id);
            debug!(peer = %peer_info,"Discovered");
            if let Some((_, m)) = self
                .peers
                .iter_mut()
                .find(|(_, c)| c.info().is_same_peer(&peer_info))
            {
                m.modify_info(|i| i.merge(peer_info.clone()));
            } else {
                self.peers.insert(
                    bit_torrent.id,
                    Peer::new(
                        peer_info.clone(),
                        tx.clone(),
                        self.progress_tx.clone(),
                        bit_torrent.supports_fast,
                        bit_torrent.supports_dht,
                        removed
                            .map(|r| r.mechanisms)
                            .unwrap_or(HashSet::from([DiscoveryMechanism::Ingoing])),
                    ),
                );
            }
        }

        if !self.should_connect_to_peer() {
            return Ok(());
        }

        if let Some(peer) = self.peers.get_mut(&bit_torrent.id) {
            peer.state.connect(tx.clone());
            if bit_torrent.supports_dht {
                let port = self.config_rx.borrow().dht_port;
                peer.send(Message::DHTPort { port }).await;
            }
        }
        if let Some(transfer) = &mut self.transfer {
            transfer
                .add_connection(peer_info.id.unwrap(), tx, bit_torrent.supports_fast, None)
                .await?;
        }
        tokio::task::Builder::new().name("BitTorrent").spawn(
            bit_torrent
                .run(self.peer_tx.clone(), rx)
                .instrument(self.span.clone()),
        )?;
        Ok(())
    }

    async fn handle_connection_event(&mut self, event: ConnectionEvent) -> Result<()> {
        use ConnectionEvent::*;
        match event {
            Connected {
                bit_torrent,
                peer_info,
                known_id,
            } => {
                self.handle_connected(bit_torrent, peer_info, known_id)
                    .await?;
            }

            ConnectionFailed(peer_info, e, known_id) => {
                self.discover(&peer_info, None)?;
                if let Some(id) = known_id {
                    debug!(peer = %peer_info, "Couldn't reconnect ({e})");
                    self.peers.entry(id).and_modify(|p| p.state.back_off());
                    self.furnish_connections()?;
                } else {
                    debug!(peer = %peer_info, "Couldn't discover ({e})");
                }
            }

            Disconnected(peer_id, error) => {
                let mut download_rate = 0.;
                if let Some(transfer) = &mut self.transfer {
                    download_rate = transfer.connection_download_rate(&peer_id).unwrap_or(0.);
                    transfer.sever_connection(&peer_id);
                }
                self.peers.entry(peer_id).and_modify(|p| {
                    debug!(peer = %p.info(), "Disconnected {error:?}");
                    p.disconnect(download_rate, error);
                });
                self.furnish_connections()?;
            }
        }
        Ok(())
    }

    async fn handle_command_event(&mut self, event: CommandEvent) -> Result<()> {
        use CommandEvent::*;
        match event {
            ToggleSequential => {
                self.progress_tx
                    .send_modify(|p| p.sequential = !p.sequential);
            }
            Reload => {
                self.tracker_event_tx.send(TrackerEvent::Reload).await?;
                let _ = self
                    .dht_tx
                    .send(DHTEvent::InfoHashRequest(
                        self.info_hash,
                        self.peer_tx.clone(),
                    ))
                    .await;
            }
            SetPriority(index, priority) => {
                self.transfer
                    .as_ref()
                    .map(|t| t.set_file_priority(index, priority));
            }
            _ => (),
        }
        Ok(())
    }

    async fn handle_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Message(peer_id, message) => {
                if let Some(peer) = self.peers.get_mut(&peer_id) {
                    let span = info_span!("peer", id = %peer_id);
                    let guard = span.enter();
                    match message {
                        Message::Metadata(msg) => {
                            self.handle_metadata_message(msg, &peer_id).await?
                        }

                        Message::ExtensionHandshake {
                            extensions,
                            client,
                            max_requests,
                            metadata_size,
                        } => {
                            debug!(
                                "Got extension handshake {extensions:?} {client:?} {max_requests:?}"
                            );
                            peer.receive_extension_handshake(client, max_requests, extensions);
                            if let Some(metadata_size) = metadata_size
                                && self.transfer.is_none()
                                && metadata_size.div_ceil(METADATA_BLOCK_SIZE)
                                    > self.metadata.num_blocks()
                            {
                                info!("Resetting metadata to {metadata_size}");
                                self.metadata.set_length_and_reset(metadata_size);
                                drop(guard);
                                self.dispatch_metadata_request().await;
                            }
                            self.transfer
                                .as_mut()
                                .map(|t| t.set_max_pipeline(&peer_id, max_requests));
                        }

                        Message::PEX(PEXMessage { added, dropped }) => {
                            debug!("Got PEX {added:?} {dropped:?}");
                            for info in added {
                                self.discover(&info, Some(DiscoveryMechanism::PEX))?;
                            }
                        }

                        Message::DHTPort { port } => {
                            debug!("DHT port {port}");
                            let addrs = futures::future::join_all(
                                peer.info()
                                    .endpoints
                                    .iter()
                                    .cloned()
                                    .map(|mut e| async move {
                                        e.set_port(port);
                                        e.resolve().await
                                    }),
                            )
                                .await
                                .into_iter()
                                .filter_map(|a| a.ok())
                                .collect();
                            let _ = self.dht_tx.send(DHTEvent::FoundNode(addrs)).await;
                        }

                        Message::Unsupported { type_byte } => {
                            warn!("Got unsupported message {type_byte}")
                        }
                        Message::UnsupportedExtension { type_byte } => {
                            warn!("Got unsupported extension message {type_byte}")
                        }

                        msg => {
                            if let Some(transfer) = &mut self.transfer {
                                transfer.handle_message(&peer_id, msg).await?;
                            } else {
                                self.peers
                                    .get_mut(&peer_id)
                                    .map(|p| p.queue_transfer_message(msg));
                            }
                        }
                    }
                }
            }

            Event::Tracker(peer_infos) => {
                debug!("Tracker discovery");
                for info in peer_infos {
                    self.discover(&info, Some(DiscoveryMechanism::Tracker))?;
                }
            }
            Event::DHT(addrs) => {
                debug!("DHT discovery");
                for addr in addrs {
                    self.discover(&PeerInfo::from(addr), Some(DiscoveryMechanism::DHT))?;
                }
            }

            Event::Connection(event) => self.handle_connection_event(event).await?,
            Event::Command(event) => self.handle_command_event(event).await?,
        }

        self.update_progress();

        Ok(())
    }
}
