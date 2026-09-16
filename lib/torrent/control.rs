use std::{
    time::Duration,
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
pub struct TransferProgress {
    pub files: Vec<FileProgress>,
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
pub struct FileProgress {
    pub relative_path: String,
    pub size: usize,
    // TODO priority
}

#[derive(Clone)]
pub struct Progress {
    pub num_discovery_attempts: usize,
    pub num_peers: usize,
    pub num_connected_peers: usize,
    pub display_name: String,
    pub metadata_down_speed: usize,
    pub metadata_up_speed: usize,
    pub metadata_bitfield: Bitfield,
    pub transfer: Option<TransferProgress>,
}

pub enum Command {
    Stop,
    Pause,
    Resume,
}
