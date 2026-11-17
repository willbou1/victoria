// TODO remember to keep an eye on dead code once in a while
#![allow(dead_code)]

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use dht::{DHTEvent, DHTManager};
use proto::bit_torrent::BitTorrent;
use tokio::fs;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::{
    select,
    sync::{Mutex, mpsc, watch},
};
use tokio_util::sync::CancellationToken;
use torrent::control::*;
use torrent::{Event, Torrent};
use tracing::{debug, error};
use types::*;

mod metainfo;
mod proto;
mod signal;
mod timer;
mod tracker;

pub mod bitfield;
pub mod dht;
pub mod torrent;
pub mod types;

pub struct TorrentHandle {
    rx: watch::Receiver<Progress>,
    tx: mpsc::Sender<Event>,
}

impl TorrentHandle {
    pub async fn delete(&self) {
        let _ = self.tx.send(Event::Delete).await;
    }

    pub async fn stop(&self) {
        let _ = self.tx.send(Event::Stop).await;
    }

    pub async fn reload(&self) {
        let _ = self.tx.send(Event::ReloadTrackers).await;
    }

    pub async fn toggle_sequential(&self) {
        let _ = self.tx.send(Event::ToggleSequential).await;
    }

    pub async fn set_priority(&self, file_index: usize, priority: FilePriority) {
        let _ = self
            .tx
            .send(Event::SetPriority(file_index, priority))
            .await;
    }

    pub fn progress(&self) -> watch::Ref<'_, Progress> {
        self.rx.borrow()
    }
}

pub struct TorrentClient {
    config_rx: watch::Receiver<Config>,
    config_tx: watch::Sender<Config>,

    cancel: CancellationToken,
    dht_tx: Option<mpsc::Sender<DHTEvent>>,
    dht_rx: Option<watch::Receiver<DHTInfo>>,

    inbound_task: Option<JoinHandle<Result<()>>>,
    dht_task: Option<JoinHandle<Result<()>>>,
    torrent_tasks: HashMap<Hash, JoinHandle<Result<()>>>,
    torrent_handles: Arc<Mutex<HashMap<Hash, Arc<TorrentHandle>>>>,
}

impl TorrentClient {
    pub async fn new(
        config_rx: watch::Receiver<Config>,
        config_tx: watch::Sender<Config>,
    ) -> Result<Self> {
        Ok(Self {
            config_rx,
            config_tx,

            cancel: CancellationToken::new(),
            dht_rx: None,
            dht_tx: None,

            inbound_task: None,
            dht_task: None,
            torrent_tasks: HashMap::new(),
            torrent_handles: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub async fn start(&mut self) -> Result<()> {
        if self.dht_task.is_none() {
            let cancel = self.cancel.clone();
            let config_rx = self.config_rx.clone();
            let torrent_handles = self.torrent_handles.clone();
            self.inbound_task = Some(
                tokio::task::Builder::new()
                    .name("Inbound")
                    .spawn(
                        async move { Self::inbound(cancel, config_rx, torrent_handles).await },
                    )?,
            );

            let dht = DHTManager::new(self.cancel.clone(), self.config_rx.clone()).await?;
            self.dht_rx = Some(dht.info_rx.clone());
            self.dht_tx = Some(dht.tx.clone());
            self.dht_task = Some(tokio::task::Builder::new().name("DHT").spawn(dht.run())?);

            let dir = &self.config_rx.borrow().data_path.clone();
            fs::create_dir_all(dir).await?;
            let mut entries = fs::read_dir(dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                if path.is_dir() {
                    self.add_existing_torrent(
                        &path.file_name().unwrap().to_string_lossy().to_string(),
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }

    pub async fn stop(&mut self) -> Result<()> {
        let Some(dht_task) = self.dht_task.take() else {
            return Ok(());
        };
        let Some(inbound_task) = self.inbound_task.take() else {
            return Ok(());
        };

        self.cancel.cancel();
        for torrent_handle in self.torrent_handles.lock().await.values() {
            torrent_handle.stop().await;
        }

        let mut errors = Vec::new();
        for (_, torrent_task) in self.torrent_tasks.drain() {
            match torrent_task.await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => errors.push(err),
                Err(err) => errors.push(anyhow!("Torrent task panicked: {err}")),
            }
        }
        match dht_task.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => errors.push(err),
            Err(err) => errors.push(anyhow!("DHT task panicked: {err}")),
        }
        match inbound_task.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => errors.push(err),
            Err(err) => errors.push(anyhow!("inbound task panicked: {err}")),
        }

        if !errors.is_empty() {
            for error in &errors {
                error!("{:#}", error);
            }
            return Err(anyhow!(
                "Errors while shutting down:\n{}",
                errors
                    .iter()
                    .map(|e| format!("{e:#}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        Ok(())
    }

    async fn inbound(
        cancel: CancellationToken,
        config_rx: watch::Receiver<Config>,
        torrent_handles: Arc<Mutex<HashMap<Hash, Arc<TorrentHandle>>>>,
    ) -> Result<()> {
        let local_id = config_rx.borrow().client_id;
        let bit_torrent_port = config_rx.borrow().bit_torrent_port;
        let bit_torrent_listener = TcpListener::bind(SocketAddr::new(
            IpAddr::from(Ipv6Addr::UNSPECIFIED),
            bit_torrent_port,
        ))
        .await?;
        loop {
            select! {
                _ = cancel.cancelled() => {
                    break;
                },
                result = bit_torrent_listener.accept() => {
                    let (socket, addr) = result?;
                    let peer_info = PeerInfo::from(Endpoint::from(addr));
                    let known_info_hashes: Vec<_> = torrent_handles.lock().await.keys().cloned().collect();
                    let bit_torrent = BitTorrent::receive_handshake(&peer_info, &known_info_hashes, &local_id, socket ).await;
                    if let Ok(bit_torrent) = bit_torrent {
                        debug!("BABYYY {bit_torrent:?}");
                        let _ = torrent_handles.lock().await.get(&bit_torrent.info_hash).unwrap()
                        .tx.send(
                            Event::Connection(bit_torrent, peer_info, None)
                        ).await;
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn add_torrent(&mut self, uri: &str) -> Result<()> {
        if let Some(dht_tx) = &self.dht_tx {
            let config_rx = self.config_rx.clone();
            let dht_tx = dht_tx.clone();
            let torrent = if uri.starts_with("magnet:?") {
                Torrent::from_magnet(uri, config_rx, dht_tx).await
            } else {
                Torrent::from_torrent_file(&PathBuf::from(uri), config_rx, dht_tx).await
            }?;

            if let Some(torrent) = torrent {
                self.add_torrent_handle(torrent).await?
            }
        }
        Ok(())
    }

    async fn add_existing_torrent(&mut self, hash: &str) -> Result<()> {
        if let Some(dht_tx) = &self.dht_tx {
            let torrent =
                Torrent::from_info_hash(hash, self.config_rx.clone(), dht_tx.clone()).await?;

            self.add_torrent_handle(torrent).await?;
        }
        Ok(())
    }

    async fn add_torrent_handle(&mut self, mut torrent: Torrent) -> Result<()> {
        let torrent_handles = &mut self.torrent_handles.lock().await;
        torrent_handles.insert(
            torrent.info_hash,
            Arc::new(TorrentHandle {
                rx: torrent.progress_rx.clone(),
                tx: torrent.peer_tx.clone(),
            }),
        );
        self.torrent_tasks.insert(
            torrent.info_hash,
            tokio::task::Builder::new()
                .name("torrent")
                .spawn(async move { torrent.run().await })?,
        );
        Ok(())
    }

    pub async fn torrent(&self, info_hash: &Hash) -> Option<Arc<TorrentHandle>> {
        self.torrent_handles
            .lock()
            .await
            .get(info_hash)
            .map(|h| h.clone())
    }

    pub async fn torrents(&self) -> HashMap<Hash, Arc<TorrentHandle>> {
        self.torrent_handles
            .lock()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    pub async fn remove_torrent(&mut self, info_hash: &Hash) -> Result<()> {
        if let Some(handle) = self.torrent_handles.lock().await.remove(info_hash) {
            handle.delete().await;
            self.torrent_tasks.remove(info_hash).unwrap().await??;
        }
        Ok(())
    }

    pub fn dht_info(&self) -> Option<watch::Ref<'_, DHTInfo>> {
        self.dht_rx.as_ref().map(|rx| rx.borrow())
    }
}
