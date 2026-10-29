use std::{
    collections::{HashMap, HashSet},
    time::Duration,
    path::PathBuf,
    array,
};

use crate::libvictoria::{
    types::*,
    bitfield::Bitfield,
};

#[derive(Debug)]
pub struct Config {
    pub client_id: PeerId,
    pub download_path: PathBuf,
    pub data_path: PathBuf,
}

#[derive(Debug)]
pub struct PieceProgress {
    pub index: usize,
    pub num_verification_failures: usize,
    pub block_bitfield: Bitfield,
}

#[derive(Debug)]
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

#[derive(Debug)]
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
    pub fn percentage(&self) -> f64 {
        self.downloaded as f64 * 100. / self.size as f64
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

#[derive(Debug)]
pub enum FilePriority {
    Low,
    Normal,
    Medium,
    High,
}

#[derive(Debug)]
pub struct FileInfo {
    pub relative_path: String,
    pub size: usize,
    pub piece_bitfield: Bitfield,
    pub priority: FilePriority,
    // TODO priority
}

#[derive(Debug)]
pub struct DiscoveryInfo {
    pub trackers: bool,
    pub pex: bool,
    pub dht: bool,
}

impl Default for DiscoveryInfo {
    fn default() -> Self {
        Self {
            dht: false,
            pex: false,
            trackers: false,
        }
    }
}

#[derive(Debug)]
pub struct PeerProgress {
    pub id: PeerId,
    pub discovery: DiscoveryInfo,
    pub client: Option<String>,
    pub supports_fast: bool,
    pub supports_pex: bool,
    pub supports_metadata: bool,
    pub supports_dht: bool,
    pub metadata_down_speed: usize,
    pub metadata_up_speed: usize,
    pub endpoints: HashSet<Endpoint>,
    pub connection: Option<ConnectionProgress>,
    pub errors: Vec<String>,
}

impl PeerProgress {
    pub fn new(id: PeerId) -> Self {
        Self {
            id,
            discovery: DiscoveryInfo::default(),
            endpoints: HashSet::new(),
            connection: None,
            client: None,
            supports_dht: false,
            supports_fast: false,
            supports_metadata: false,
            supports_pex: false,
            metadata_down_speed: 0,
            metadata_up_speed: 0,
            errors: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub enum TrackerStatus {
    Announcing,
    Failed,
    Succeeded,
}

#[derive(Debug)]
pub struct TrackerInfo {
    pub url: String,
    pub tier: usize,
    pub succeeded: Option<TrackerStatus>,
    pub seeders: Option<usize>,
    pub leechers: Option<usize>,
    pub interval: Option<Duration>,
    pub min_interval: Option<Duration>,
    pub endpoints: Option<HashSet<Endpoint>>,
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
            endpoints: None,
        }
    }
}

#[derive(Debug)]
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

#[derive(Debug)]
pub enum Command {
    Stop,
    Pause,
    Resume,
    Delete,
    ReloadTrackers,
}

#[derive(Debug)]
pub struct DHTNodeInfo {

}

#[derive(Debug)]
pub struct DHTBucketInfo {
    pub nodes: Vec<DHTNodeInfo>,
}

impl Default for DHTBucketInfo {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub struct DHTInfo {
    pub buckets: [DHTBucketInfo; 160],
}

impl Default for DHTInfo {
    fn default() -> Self {
        Self {
            buckets: array::from_fn(|_| DHTBucketInfo::default()),
        }
    }
}
