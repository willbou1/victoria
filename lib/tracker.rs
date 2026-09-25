use rand::seq::SliceRandom;
use anyhow::Result;
use tracing::{Instrument, debug, info, trace, warn};
use std::{
    fmt,
    time::{Duration},
};
use tokio::sync::{
    mpsc,
    watch,
};

use crate::{
    torrent,
    torrent::control::*,
    proto::tracker::{Event, TrackerResponse, request_http, request_udp, self},
    types::*,
};

pub struct Trackers {
    urls: Vec<Vec<String>>,
    info_hash: Hash,
    client_id: PeerId,
    tx: mpsc::Sender<torrent::Event>,
    rx: watch::Receiver<tracker::Progress>,
    progress_tx: watch::Sender<Progress>,

    pub interval: Option<u64>,
    pub min_interval: Option<u64>,
    pub seeders: Option<u64>,
    pub leechers: Option<u64>,
    pub peers: Vec<PeerInfo>,

    progress: tracker::Progress,
}

impl Trackers {
    pub fn new(
        tx: mpsc::Sender<torrent::Event>,
        rx: watch::Receiver<tracker::Progress>,
        progress_tx: watch::Sender<Progress>,
        client_id: PeerId,
        info_hash: Hash,
        mut urls: Vec<Vec<String>>,
    ) -> Self {
        for (t, tier_urls) in urls.iter_mut().enumerate() {
            for url in &*tier_urls {
                progress_tx.send_modify(|p| {
                    p.trackers.entry(url.clone()).or_insert(TrackerInfo::new(t, url.clone()));
                });
            }
            tier_urls.shuffle(&mut rand::rng());
        }
        
        Self {
            urls,
            info_hash,
            client_id,
            tx,
            rx,
            progress_tx,

            interval: None,
            min_interval: None,
            seeders: None,
            leechers: None,
            peers: Vec::new(),

            progress: tracker::Progress::default(),
        }
    }

    pub async fn run(mut self) {
        let span = tracing::info_span!("trackers");
        let _enter = span.enter();
        async {
            self.announce().await;

            let mut sleep = Box::pin(tokio::time::sleep(Duration::from_secs(
                self.interval.unwrap_or(60)
                    .max(self.min_interval.unwrap_or(0))
            )));
            loop {
                tokio::select!(
                    _ = &mut sleep => {
                        self.announce().await;

                        sleep.as_mut().reset(
                            tokio::time::Instant::now()
                            + Duration::from_secs(
                                self.interval.unwrap_or(60)
                                .max(self.min_interval.unwrap_or(0))
                            )
                        );
                    }

                    result = self.rx.changed() => match result {
                        Ok(()) => self.progress = self.rx.borrow().clone(),
                        Err(_) => {
                            trace!("Quitting tracker loop");
                            return;
                        }
                    }
                );
            }
        }.instrument(span.clone()).await
    }

    fn reset(&mut self) {
        self.interval = None;
        self.min_interval = None;
        self.seeders = None;
        self.leechers = None;
    }

    async fn announce(&mut self) {
        const ANNOUNCE_TO_ALL_TIERS: bool = true;
        let mut discovered = false;

        for tier_urls in self.urls.iter_mut() {
            for url in &*tier_urls {
                    self.progress_tx.send_modify(|p| {
                        p.trackers.entry(url.clone()).and_modify(|t| {
                            t.succeeded = None;
                        });
                    });
            }
        }

        for t in 0..self.urls.len() {
            if !ANNOUNCE_TO_ALL_TIERS {
                self.reset();
            }
            
            let mut good = Vec::new();
            let mut bad = Vec::new();
            for u in 0..self.urls[t].len() {
                let url = self.urls[t][u].clone();
                let span = tracing::info_span!(
                    "tracker",
                    url = %url
                );
                async {
                    match match url.split(":").next().unwrap() {
                        "http" | "https" => request_http(
                            &url,
                            &self.client_id,
                            &self.info_hash,
                            &self.progress,
                        ).await,
                        "udp" => match tokio::time::timeout(
                            Duration::from_secs(10),
                            request_udp(
                                &url,
                                &self.client_id,
                                &self.info_hash,
                                &self.progress,
                            ),
                        ).await {
                            Ok(response) => response,
                            Err(_) => Err(anyhow::anyhow!("Exceeded tracker manager timeout for UDP")),
                        },
                        proto => Err(anyhow::anyhow!("Unsopported protocol {proto}")),
                    } {
                        Ok(response) => {
                            self.progress_tx.send_modify(|p| {
                                p.trackers.entry(url.clone()).and_modify(|t| {
                                    t.succeeded = Some(true);
                                    t.seeders = response.seeders.map(|s| s as usize);
                                    t.leechers = response.leechers.map(|l| l as usize);
                                    t.interval = Some(Duration::from_secs(response.interval));
                                    t.min_interval = response.min_interval.map(|i| Duration::from_secs(i));
                                    t.endpoints = Some(response.peers.iter()
                                        .map(|p| p.endpoints.iter().cloned().next().unwrap()).collect());
                                });
                            });
                            self.update(response);
                            debug!("Successfully announced");
                            good.push(url);
                            discovered = true;
                        }
                        Err(e) => {
                            self.progress_tx.send_modify(|p| {
                                p.trackers.entry(url.clone()).and_modify(|t| {
                                    t.succeeded = Some(false);
                                });
                            });
                            debug!("Failed to announced {e}");
                            bad.push(url);
                        }
                    }
                    let _ = self.tx.send(torrent::Event::Tracker(self.peers.clone())).await;
                }.instrument(span).await
            }
            good.extend(bad);
            self.urls[t] = good;

            if discovered {
                debug!(tier = &t, "Successfully announced to tier tier\n{self}");
                if !ANNOUNCE_TO_ALL_TIERS {
                    return;
                }
            }
        }
        debug!("Successfully announced to all tiers");
    }

    fn update(&mut self, response: TrackerResponse) {
        trace!("Got tracker response:\n{response}");
        
        if let Some(interval) = self.interval {
            self.interval = Some(response.interval.min(interval));
        } else {
            self.interval = Some(response.interval);
        }

        self.min_interval = response.min_interval.max(self.min_interval);
        self.seeders = response.seeders.max(self.seeders);
        self.leechers = response.leechers.max(self.leechers);

        for peer in response.peers {
            if let Some(m) = self.peers.iter_mut().find(|m| m.is_same_peer(&peer)) {
                m.merge(peer);
            } else {
                self.peers.push(peer);
            }
        }
    }
}

impl fmt::Display for Trackers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Interval: {}", self.interval.unwrap_or(60))?;
        if let Some(min_interval) = self.min_interval {
            writeln!(f, "Minimum interval: {}", min_interval)?;
        }
        if let Some(complete) = self.seeders {
            writeln!(f, "Complete: {}", complete)?;
        }
        if let Some(incomplete) = self.leechers {
            writeln!(f, "Incomplete: {}", incomplete)?;
        }
        writeln!(f, "Peers:")?;
        for peer in &self.peers {
            writeln!(f, "    {}", peer)?;
        }
        Ok(())
    }
}
