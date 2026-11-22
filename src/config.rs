use anyhow::{Result, anyhow};
use directories::{BaseDirs, UserDirs};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::fs;

use crate::libvictoria::types::*;

#[derive(Debug, Deserialize, Serialize)]
pub struct AppConfig {
    pub download_path: PathBuf,
    pub data_path: PathBuf,

    pub bit_torrent: BitTorrentConfig,
    pub dht: DHTConfig,
}

impl AppConfig {
    pub async fn read() -> Result<Self> {
        let base_dirs = BaseDirs::new().ok_or(anyhow!("Can't get base directories"))?;
        let user_dirs = UserDirs::new().ok_or(anyhow!("Can't get user directories"))?;
        let config_path = base_dirs.config_dir().join("victoria").join("config.toml");
        if !fs::try_exists(&config_path).await? {
            let config = AppConfig {
                data_path: base_dirs.data_dir().join("victoria").join("torrents"),
                download_path: user_dirs
                    .download_dir()
                    .ok_or(anyhow!("Can't find download directory"))?
                    .join("victoria"),
                bit_torrent: BitTorrentConfig::default(),
                dht: DHTConfig::default(),
            };
            return Ok(config);
        }
        let config_buffer = fs::read_to_string(config_path).await?;
        let config = toml::from_str(&config_buffer)?;
        Ok(config)
    }

    pub async fn write(&self) -> Result<()> {
        let config_path = BaseDirs::new()
            .ok_or(anyhow!("Can't get base directories"))?
            .config_dir()
            .join("victoria")
            .join("config.toml");
        let config_buffer = toml::to_string(self)?;
        fs::write(&config_path, config_buffer).await?;
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct BitTorrentConfig {
    pub id: String,
    pub port: u16,
    pub connection_cap: usize,
    pub discovery_cap: usize,
}

impl Default for BitTorrentConfig {
    fn default() -> Self {
        Self {
            id: PeerId::random().to_string(),
            port: 6881,
            connection_cap: 10,
            discovery_cap: 100,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct DHTConfig {
    pub id: String,
    pub port: u16,
}

impl Default for DHTConfig {
    fn default() -> Self {
        Self {
            id: NodeId::random().to_string(),
            port: 6881,
        }
    }
}
