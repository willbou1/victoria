use std::{
    collections::{HashMap, HashSet},
    time::Duration
};

use crate::{
    types::*,
    bitfield::Bitfield,
};

#[derive(Clone)]
pub struct PieceProgress {
    pub index: usize,
    pub num_blocks: usize,
    pub num_obtained_blocks: usize,
    pub block_bitfield: Bitfield,
}

#[derive(Clone)]
pub struct ConnectionProgress {
    pub down_speed: usize,
    pub up_speed: usize,
    pub piece_bitfield: Bitfield,
    pub am_choking: bool,
    pub am_interested: bool,
    pub peer_choking: bool,
    pub peer_interested: bool,
    pub pipeline: usize,
    pub piece_cursor: Option<usize>,
    pub timeout_rate: usize,
    pub reject_rate: usize,
}

#[derive(Clone)]
pub struct TransferProgress {
    pub files: Vec<FileInfo>,
    pub down_speed: usize,
    pub up_speed: usize,
    pub downloaded: usize,
    pub uploaded: usize,
    pub size: usize,
    pub piece_bitfield: Bitfield,
    pub active_pieces: Vec<PieceProgress>,
}

impl TransferProgress {
    pub fn percentage(&self) -> f32 {
        self.downloaded as f32 * 100. / self.size as f32
    }

    pub fn eta(&self) -> Option<Duration> {
        if self.down_speed == 0 {
            return None;
        }
        Some(Duration::from_secs(
            (self.size - self.downloaded) as u64 / self.down_speed as u64
        ))
    }
}

#[derive(Clone)]
pub struct FileInfo {
    pub relative_path: String,
    pub size: usize,
    // TODO priority
}

#[derive(Clone)]
pub struct PeerProgress {
    pub id: PeerId,
    pub client: Option<String>,
    pub supports_fast: bool,
    pub supports_pex: bool,
    pub supports_metadata: bool,
    pub supports_dht: bool,
    pub metadata_down_speed: usize,
    pub metadata_up_speed: usize,
    pub endpoints: HashSet<PeerEndpoint>,
    pub connection: Option<ConnectionProgress>,
}

impl PeerProgress {
    pub fn new(id: PeerId) -> Self {
        Self {
            id,
            endpoints: HashSet::new(),
            connection: None,
            client: None,
            supports_dht: false,
            supports_fast: false,
            supports_metadata: false,
            supports_pex: false,
            metadata_down_speed: 0,
            metadata_up_speed: 0,
        }
    }
}

#[derive(Clone)]
pub struct TrackerInfo {
    pub url: String,
    pub tier: usize,
    pub succeeded: Option<bool>,
    pub seeders: Option<usize>,
    pub leechers: Option<usize>,
    pub interval: Option<Duration>,
    pub min_interval: Option<Duration>,
}

impl TrackerInfo {
    pub fn new(tier: usize, url: String) -> Self {
        Self {
            url,
            tier,
            succeeded: None,
            seeders: None,
            leechers: None,
            interval: None,
            min_interval: None,
        }
    }
}

#[derive(Clone)]
pub struct Progress {
    pub num_discovery_attempts: usize,
    pub num_peers: usize,
    pub num_connected_peers: usize,
    pub display_name: String,
    pub metadata_bitfield: Bitfield,
    pub transfer: Option<TransferProgress>,
    pub peers: HashMap<PeerId, PeerProgress>,
    pub trackers: HashMap<String, TrackerInfo>,
}

pub enum Command {
    Stop,
    Pause,
    Resume,
}
