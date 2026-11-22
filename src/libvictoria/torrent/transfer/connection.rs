use std::time::Duration;
use tokio::sync::{mpsc, watch};

use crate::libvictoria::{
    bitfield::Bitfield,
    proto::bit_torrent::Message,
    signal::AsymmetricEMA,
    torrent::control::{ConnectionProgress, Progress},
    types::*,
};

const UPLOAD_LIMIT: usize = 32 * 1024; // b/s per peer

const TIMEOUTS_BOOST_TAU: Duration = Duration::from_millis(1250);
const TIMEOUTS_THROTTLE_TAU: Duration = Duration::from_secs(5);
const DOWNLOAD_RATE_TAU: Duration = Duration::from_secs(20);

const DEFAULT_PIPELINE: usize = 20;
const MIN_PIPELINE: usize = 2;
const DEFAULT_MAX_PIPELINE: usize = 200;
const PIPELINE_STEP: usize = 2;
const PIPELINE_THROTTLE_FACTOR: f32 = 0.8;

pub struct Connection {
    id: PeerId,

    pub supports_fast: bool,
    piece_cursor: Option<usize>,
    tx: mpsc::Sender<Message>,
    progress_tx: watch::Sender<Progress>,

    sent_requests: usize,
    pipeline: usize,
    max_pipeline: usize,

    am_choking: bool,
    am_interested: bool,
    peer_choking: bool,
    peer_interested: bool,
    piece_bitfield: Option<Bitfield>,

    uploaded_this_second: usize,

    downloaded_this_second: usize,
    response_times_sum: Duration,
    num_response_times: usize,
    chokes_this_second: usize,
    rejects_this_second: usize,
    timeouts_this_second: usize,

    timeouts_ema: AsymmetricEMA,
    download_rate_ema: AsymmetricEMA,
}

impl Connection {
    pub fn new(
        id: PeerId,
        tx: mpsc::Sender<Message>,
        progress_tx: watch::Sender<Progress>,
        supports_fast: bool,
    ) -> Self {
        let con = Self {
            id,
            supports_fast,
            sent_requests: 0,
            piece_cursor: None,
            tx,
            progress_tx,
            max_pipeline: DEFAULT_MAX_PIPELINE,
            pipeline: DEFAULT_PIPELINE,

            am_choking: true,
            am_interested: false,
            peer_choking: true,
            peer_interested: false,
            piece_bitfield: None,

            uploaded_this_second: 0,
            downloaded_this_second: 0,
            num_response_times: 0,
            response_times_sum: Duration::default(),
            chokes_this_second: 0,
            rejects_this_second: 0,
            timeouts_this_second: 0,

            timeouts_ema: AsymmetricEMA::new(
                Duration::from_secs(1),
                TIMEOUTS_BOOST_TAU,
                TIMEOUTS_THROTTLE_TAU,
                None,
            ),
            download_rate_ema: AsymmetricEMA::new(
                Duration::from_secs(1),
                DOWNLOAD_RATE_TAU,
                DOWNLOAD_RATE_TAU,
                None,
            ),
        };
        con.init_progress();
        con
    }

    fn init_progress(&self) {
        self.progress_tx.send_modify(|p| {
            p.peers.entry(self.id).and_modify(|peer| {
                peer.connection = Some(ConnectionProgress {
                    down_speed: self.downloaded_this_second(),
                    up_speed: self.uploaded_this_second(),
                    piece_bitfield: self.piece_bitfield.clone(),
                    am_choking: self.am_choking(),
                    am_interested: self.am_interested(),
                    peer_choking: self.peer_choking(),
                    peer_interested: self.peer_interested(),
                    pipeline: self.pipeline,
                    max_pipeline: self.max_pipeline,
                    piece_cursor: self.piece_cursor,
                    timeout_rate: self.timeouts_this_second,
                    reject_rate: self.rejects_this_second,
                })
            });
        });
    }

    fn notify(&self, f: impl Fn(&mut ConnectionProgress)) {
        self.progress_tx.send_modify(|p| {
            p.peers
                .get_mut(&self.id)
                .and_then(|p| p.connection.as_mut())
                .map(|c| f(c));
        });
    }

    pub fn can_request(&self) -> bool {
        self.sent_requests < self.pipeline && !self.peer_choking
    }

    pub fn can_upload(&self) -> bool {
        self.uploaded_this_second < UPLOAD_LIMIT && !self.am_choking
    }

    pub fn piece_cursor(&self) -> Option<usize> {
        self.piece_cursor
    }
    pub fn download_rate(&self) -> f64 {
        self.download_rate_ema.value()
    }
    pub fn uploaded_this_second(&self) -> usize {
        self.uploaded_this_second
    }
    pub fn downloaded_this_second(&self) -> usize {
        self.downloaded_this_second
    }
    pub fn sent_requests(&self) -> usize {
        self.sent_requests
    }
    pub fn am_choking(&self) -> bool {
        self.am_choking
    }
    pub fn am_interested(&self) -> bool {
        self.am_interested
    }
    pub fn peer_choking(&self) -> bool {
        self.peer_choking
    }
    pub fn peer_interested(&self) -> bool {
        self.peer_interested
    }
    pub fn piece_bitfield(&self) -> Option<&Bitfield> {
        self.piece_bitfield.as_ref()
    }

    pub fn set_piece_cursor(&mut self, piece_cursor: Option<usize>) {
        self.piece_cursor = piece_cursor;
        self.notify(|c| c.piece_cursor = self.piece_cursor);
    }

    pub fn set_max_pipeline(&mut self, max_pipeline: Option<usize>) {
        self.max_pipeline = max_pipeline.unwrap_or(DEFAULT_MAX_PIPELINE);
        self.notify(|c| c.max_pipeline = self.max_pipeline);
    }
    pub fn boost_pipeline(&mut self) {
        self.pipeline = (self.pipeline + PIPELINE_STEP).min(self.max_pipeline);
        self.notify(|c| c.pipeline = self.pipeline);
    }
    pub fn throttle_pipeline(&mut self) {
        self.pipeline = (self.pipeline as f32 * PIPELINE_THROTTLE_FACTOR) as usize;
        self.pipeline = self.pipeline.max(MIN_PIPELINE);
        self.notify(|c| c.pipeline = self.pipeline);
    }

    pub fn sub_sent_requests(&mut self, count: usize) {
        self.sent_requests = self.sent_requests.saturating_sub(count);
    }
    pub fn reset_sent_requests(&mut self) {
        self.sent_requests = 0;
    }

    pub async fn set_am_choking(&mut self, choking: bool) {
        self.am_choking = choking;
        self.send(Message::Choked(choking)).await;
        self.notify(|c| c.am_choking = choking);
    }

    pub async fn set_am_interested(&mut self, interested: bool) {
        self.am_interested = interested;
        self.send(Message::Interested(interested)).await;
        self.notify(|c| c.am_interested = interested);
    }

    pub fn set_peer_choking(&mut self, choking: bool) {
        self.peer_choking = choking;
        if choking {
            self.chokes_this_second += 1;
            if !self.supports_fast {
                self.reset_sent_requests();
            }
        }
        self.notify(|c| c.peer_choking = choking);
    }

    pub fn set_peer_interested(&mut self, interested: bool) {
        self.peer_interested = interested;
        self.notify(|c| c.peer_interested = interested);
    }

    pub fn set_bitfield(&mut self, bitfield: Bitfield) {
        self.piece_bitfield = Some(bitfield);
        self.notify(|c| c.piece_bitfield = self.piece_bitfield.clone());
    }
    pub fn set_full_bitfield(&mut self, num_pieces: usize) {
        let mut bitfield = Bitfield::new(num_pieces);
        bitfield.fill(true);
        self.set_bitfield(bitfield);
    }
    pub fn set_empty_bitfield(&mut self, num_pieces: usize) {
        let bitfield = Bitfield::new(num_pieces);
        self.set_bitfield(bitfield);
    }
    pub fn has_piece(&self, index: usize) -> bool {
        self.piece_bitfield
            .as_ref()
            .map(|b| b.is_set(index))
            .unwrap_or(false)
    }
    pub fn set_piece(&mut self, index: usize) -> bool {
        if let Some(piece_bitfield) = &mut self.piece_bitfield {
            let set = piece_bitfield.set(index);
            self.notify(|c| {
                c.piece_bitfield.as_mut().map(|b| b.set(index));
            });
            return set;
        }
        false
    }

    pub fn reject(&mut self) {
        self.rejects_this_second += 1;
        self.sub_sent_requests(1);
    }

    pub fn timeout(&mut self, count: usize) {
        self.timeouts_this_second += count;
        self.sub_sent_requests(count);
    }

    pub async fn send(&self, message: Message) {
        let _ = self.tx.send(message).await;
    }

    pub async fn request(&mut self, index: usize, begin: usize, length: usize) {
        self.send(Message::Request {
            index,
            begin,
            length,
        })
        .await;
        self.sent_requests += 1;
    }

    pub fn receive_piece(
        &mut self,
        length: usize,
        response_time: Option<Duration>,
        is_downloading: bool,
    ) {
        if is_downloading {
            self.sub_sent_requests(1);
        }
        self.downloaded_this_second += length;
        if let Some(response_time) = response_time {
            self.response_times_sum += response_time;
            self.num_response_times += 1;
        }
    }

    pub async fn send_piece(&mut self, index: usize, begin: usize, data: Vec<u8>) {
        self.uploaded_this_second += data.len();
        self.send(Message::Piece {
            index,
            begin,
            data,
            response_time: None,
        })
        .await;
    }

    fn control_congestion(&mut self) {
        self.timeouts_ema.update(self.timeouts_this_second as f64);

        if self.timeouts_ema.value() >= 1. {
            if !self.peer_choking {
                self.throttle_pipeline();
            }
        } else if self.downloaded_this_second != 0 {
            self.boost_pipeline();
        }
    }

    pub fn reset_stats(&mut self) {
        self.control_congestion();
        if self.chokes_this_second == 0 {
            self.download_rate_ema
                .update(self.downloaded_this_second as f64);
        }
        self.notify(|c| {
            c.timeout_rate = self.timeouts_this_second;
            c.reject_rate = self.rejects_this_second;
            c.down_speed = self.downloaded_this_second;
            c.up_speed = self.uploaded_this_second;
        });

        self.uploaded_this_second = 0;
        self.downloaded_this_second = 0;
        self.num_response_times = 0;
        self.response_times_sum = Duration::default();
        self.chokes_this_second = 0;
        self.rejects_this_second = 0;
        self.timeouts_this_second = 0;
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.progress_tx.send_modify(|p| {
            p.peers
                .entry(self.id)
                .and_modify(|peer| peer.connection = None);
        });
    }
}
