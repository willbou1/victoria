mod connection;
mod piece_cache;

use anyhow::{Result, ensure};
use tokio::{
    fs::{self, File},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    sync::{mpsc, watch},
};
use std::{
    collections::{HashMap, VecDeque, HashSet},
    io::SeekFrom,
};
use rand::{rng, seq::IndexedRandom};
use tracing::{info_span, info, debug, trace};

use crate::libvictoria::{
    bitfield::Bitfield,
    metainfo::Metadata,
    proto::{
        bit_torrent::Message,
        tracker,
    },
    torrent::{
        PieceProgress,
        piece::Piece,
        control::*
    },
    types::*,
};
use connection::Connection;
use piece_cache::PieceCache;

pub struct Transfer {
    pub(crate) metadata: Metadata,
    info_hash: Hash,
    tracker_tx: watch::Sender<tracker::Progress>,
    progress_tx: watch::Sender<Progress>,
    config_rx: watch::Receiver<Config>,
    pieces: Vec<Piece>,
    piece_bitfield: Bitfield,
    priority_cursor: usize,
    connections: HashMap<PeerId, Connection>,
    torrent_files: Vec<File>,
    piece_cache: PieceCache,
    uploaded: usize,
}

impl Transfer {
    const PIEC_CACHE_CAPACITY: usize = 8;
    const PRIORITIES: [FilePriority; 10] = {use FilePriority::*; [
        Low,
        Normal, Normal,
        Medium, Medium, Medium,
        High, High, High, High,
    ]};
    const BLOCK_SIZE: usize = 16 * 1024;
    fn block_index(begin: usize) -> usize {begin / Self::BLOCK_SIZE}

    pub async fn new(
        metadata: Metadata,
        info_hash: Hash,
        tracker_tx: watch::Sender<tracker::Progress>,
        progress_tx: watch::Sender<Progress>,
        config_rx: watch::Receiver<Config>,
    ) -> Result<Self> {
        let mut files = Vec::new();
        let base_path = config_rx.borrow().download_path.clone();
        fs::create_dir_all(&base_path).await?;
        for file in &metadata.files {
            let path = base_path.join(&file.path);
            fs::create_dir_all(&path.parent().unwrap()).await?;
            let torrent_file = fs::OpenOptions::new()
                .read(true).write(true).create(true)
                .open(&path).await?;
            torrent_file.set_len(file.length as u64).await?;
            files.push(torrent_file);
        }

        let (pieces, piece_bitfield)
            = Self::load_state(&metadata, &info_hash, &config_rx, &mut files).await?;
        let downloaded = (piece_bitfield.num_set() * metadata.piece_length).min(metadata.length);
        let left = metadata.length - downloaded;
        let _ = tracker_tx.send(tracker::Progress {
            downloaded,
            left,
            event: tracker::Event::Started,
            uploaded: 0,
        });

        progress_tx.send_modify(|p| p.transfer = Some(
            TransferProgress::new(&metadata, piece_bitfield.clone(),
                pieces.iter().enumerate()
                    .filter(|(_, p)| p.is_active())
                    .map(|(p, piece)| PieceProgress {
                        index: p,
                        block_bitfield: piece.to_bitfield(),
                        num_verification_failures: piece.num_verification_failures(),
                    })
                    .collect()
            )
        ));

        Ok(Self {
            info_hash,
            piece_cache: PieceCache::new(Self::PIEC_CACHE_CAPACITY),
            torrent_files: files,
            pieces,
            priority_cursor: 0,
            metadata,
            tracker_tx,
            progress_tx,
            config_rx,
            piece_bitfield,
            connections: HashMap::new(),
            uploaded: 0,
        })
    }

    pub async fn save_state(&self) -> Result<()> {
        info!("Saving");
        let path = self.config_rx.borrow().data_path
            .join(self.info_hash.to_string()).join("state");
        fs::create_dir_all(&path.parent().unwrap()).await?;
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path).await?;
        file.write_all(self.piece_bitfield.as_bytes()).await?;
        for (p, piece) in self.pieces.iter().filter(|p| p.is_active()).enumerate() {
            file.write_u64_le(p as u64).await?;
            file.write_all(piece.to_bitfield().as_bytes()).await?;
        }
        Ok(())
    }

    pub async fn add_connection(
        &mut self,
        peer_id: PeerId,
        tx: mpsc::Sender<Message>,
        supports_fast: bool,
        initial_messages: Option<&mut VecDeque<Message>>,
    ) -> Result<()> {
        let mut con = Connection::new(peer_id, tx, self.metadata.num_pieces, supports_fast);
        if self.piece_bitfield.num_set() == 0 && supports_fast {
            con.send(Message::HaveNone).await;
        } else if self.piece_bitfield.num_set() == self.metadata.num_pieces && supports_fast {
            con.send(Message::HaveAll).await;
        } else if self.piece_bitfield.num_set() != 0 {
            con.send(Message::Bitfield(self.piece_bitfield.clone())).await;
        }
        con.set_am_interested(true).await;
        self.connections.insert(peer_id, con);

        if let Some(initial_messages) = initial_messages {
            for msg in initial_messages.drain(..) {
                trace!("Read initial transfer message {msg:?}");
                self.handle_event(&peer_id, msg).await?;
            }
        }
        Ok(())
    }

    pub fn sever_connection(&mut self, peer_id: &PeerId) {
        for piece in self.pieces.iter_mut() {
            piece.reset(&peer_id);
        }
        self.progress_tx.send_modify(|p| {
            p.peers.entry(*peer_id).and_modify(
                |peer| peer.connection = None
            );
        });
        self.connections.remove(peer_id);
    }
    
    pub async fn tick(&mut self) -> Result<()> {
        self.check_request_accounting("Before timeouts");
        for piece in self.pieces.iter_mut() {
            for (peer_id, count) in piece.check_timeout() {
                self.connections.entry(peer_id).and_modify(|c| c.timeout(count));
            }
        }
        self.check_request_accounting("After timeouts");

        self.statistics();

        self.check_request_accounting("Before tick dispatch");
        self.dispatch_requests().await?;
        self.check_request_accounting("After tick dispatch");
        Ok(())
    }

    // private
    fn check_request_accounting(&self, when: &str) {
        // This where request leaking gets caught like a lil bitch
        #[cfg(debug_assertions)] { 
            let mut nums: HashMap<PeerId, usize> =
                self.connections.keys().map(|id| (*id, 0)).collect();
            for piece in &self.pieces {
                for (peer_id, count) in piece.num_downloading() {
                    *nums.entry(peer_id).or_insert(0) += count;
                }
            }
            for (peer_id, count) in nums {
                if let Some(con) = self.connections.get(&peer_id) {
                    debug_assert_eq!(
                        con.sent_requests(), count,
                        "{peer_id}: connection RQ={}, piece downloading={count}, when={when}",
                        con.sent_requests(),
                    );
                }
            }
        }

        #[cfg(not(debug_assertions))]
        let _ = when;
    }
    
    pub async fn handle_event(
        &mut self,
        peer_id: &PeerId,
        message: Message
    ) -> Result<()> {
        let span = info_span!("connection", id = %peer_id);
        let guard = span.enter();
        match message {
            Message::Bitfield(bitfield) => {
                debug!("Set bitfield {:?}", bitfield.as_bytes());
                if let Some(con) = self.connections.get_mut(peer_id) {
                    con.set_bitfield(bitfield)?;
                }
            }

            Message::Have { index } => {
                debug!("Got have {index}");
                self.check_piece_index(index, "have")?;
                self.connections.entry(*peer_id).and_modify(|c| c.set_piece(index));
            }

            Message::Request { index, begin, length } => {
                self.check_piece_length(index, begin, length, false, "request")?;
                let block_index = Self::block_index(begin);
                debug!("Got request {index}:{block_index}");
                if self.connections.get(peer_id).map(|c| c.can_upload()).unwrap_or(false)
                    && self.piece_bitfield.is_set(index) {
                        let piece = self.read_piece(index, begin, length).await?;
                        debug!("Sent block {index}:{block_index} ({})", piece.len());
                        if let Some(c) = self.connections.get_mut(peer_id) {
                            c.send_piece(index, begin, piece).await;
                        }
                    } else if let Some(c) = self.connections.get_mut(peer_id) && c.supports_fast {
                        c.send(Message::Reject { index, begin, length }).await;
                    } else {
                        // maybe queue this shit to send it later
                    }
            }

            Message::Cancel { index, begin, length } => {
                self.check_piece_length(index, begin, length, true, "cancel")?;
                let block_index = Self::block_index(begin);
                debug!("Got cancel {index}:{block_index}");
            }

            Message::Piece { index, begin, piece: block, response_time } => {
                self.check_piece_begin(index, begin, true, "piece")?;
                let block_index = Self::block_index(begin);

                // maybe keep track of later pieces just for response time
                if !self.pieces[index].has_obtrined(block_index) {
                    debug!("Got block {index}:{block_index}");
                    let who_downloading = self.pieces[index].who_downloading(block_index);
                    self.connections.entry(*peer_id).and_modify(
                        |c| c.receive_piece(
                            block.len(),
                            response_time,
                            who_downloading.map(|w| w == peer_id).unwrap_or(false))
                    );

                    if let Some(who) = who_downloading && who != peer_id {
                        self.connections.get_mut(who).map(|c| c.sub_sent_requests(1));
                    }

                    self.write_block(index, block_index, &block).await?;
                    if self.pieces[index].place(block_index, block, true).is_some() {
                        self.piece_bitfield.set(index);
                        for con in self.connections.values() {
                            con.send(Message::Have {index}).await;
                        }
                        let (downloaded, left) = self.downloaded_left();
                        let _ = self.tracker_tx.send(tracker::Progress {
                            downloaded,
                            left,
                            event: tracker::Event::Started,
                            uploaded: self.uploaded,
                        });
                    }
                    
                    self.progress_tx.send_modify(|p| {
                        if let Some(t) = p.transfer.as_mut() {
                            for (f, file) in self.metadata.files.iter().enumerate() {
                                t.files[f].piece_bitfield = self.piece_bitfield.subfield(file.pieces);
                            }
                            t.downloaded = self.downloaded_left().0;
                            t.piece_bitfield = self.piece_bitfield.clone();
                            t.active_pieces = self.pieces.iter()
                                .enumerate()
                                .filter(|(_, p)| p.is_active())
                                .map(|(p, piece)| PieceProgress {
                                    index: p,
                                    block_bitfield: piece.to_bitfield(),
                                    num_verification_failures: piece.num_verification_failures(),
                                })
                                .collect();
                        }
                    });
                }
            }

            Message::Choked(choked) => {
                debug!("Set choke {choked}");
                if let Some(con) = self.connections.get_mut(peer_id) {
                    con.set_peer_choking(choked);
                    if choked && !con.supports_fast {
                        // All pending requests are considered invalidated
                        for piece in self.pieces.iter_mut() {
                            piece.reset(&peer_id);
                        }
                    }
                }
            }
            Message::Interested(interested) => {
                if let Some(con) = self.connections.get_mut(peer_id) {
                    con.set_peer_interested(interested);
                    con.set_am_choking(false).await;
                }
            }

            // extensions
            Message::Reject { index, begin, length } => {
                self.check_piece_length(index, begin, length, true, "reject")?;
                let block_index = Self::block_index(begin);
                debug!("Got rejection for block {index}:{block_index}");
                // TODO implement BEP semantics properly
                let who_downloading = self.pieces[index].who_downloading(block_index);
                if let Some(who) = who_downloading && who == peer_id {
                    self.connections.entry(*peer_id).and_modify(|c| c.reject());
                    self.pieces[index].reject(block_index, *peer_id);
                }
            }
            Message::HaveAll => {
                debug!("Got have all");
                self.connections.entry(*peer_id).and_modify(|c| c.set_pieces());
            },
            Message::HaveNone => {
                debug!("Got have none");
                self.connections.entry(*peer_id).and_modify(|c| c.unset_pieces());
            },
            Message::Suggest { index } => {
                debug!("Got suggest for block {index}");
            },
            Message::AllowedFast { .. } => (),

            _ => (),
        }

        self.check_request_accounting("Before dispatch");
        drop(guard);
        self.dispatch_requests().await?;
        self.check_request_accounting("After dispatch");

        Ok(())
    }

    async fn load_state(
        metadata: &Metadata,
        info_hash: &Hash,
        config_rx: &watch::Receiver<Config>,
        torrent_files: &mut [File],
    ) -> Result<(Vec<Piece>, Bitfield)> {
        let mut pieces: Vec<_> =
            std::iter::repeat_with(|| None)
                .take(metadata.num_pieces)
                .collect();
        let path = config_rx.borrow().data_path
            .join(info_hash.to_string()).join("state");
        let mut bitfield = Bitfield::new(metadata.num_pieces);
        if fs::try_exists(&path).await? {
            info!("Recovering");
            let mut file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .open(&path).await?;
            let mut bytes = vec![0; metadata.num_pieces.div_ceil(8)];
            file.read_exact(&mut bytes).await?;
            bitfield.set_bytes(&bytes);
            let mut active = HashSet::new();
            let mut block_buffer = vec![0; metadata.piece_length.div_ceil(Self::BLOCK_SIZE).div_ceil(8)];
            while let Ok(p) = file.read_u64_le().await.map(|p| p as usize) {
                let num_blocks = metadata.piece_length(p).div_ceil(Self::BLOCK_SIZE);
                file.read_exact(&mut block_buffer[..num_blocks.div_ceil(8)]).await?;
                let mut block_bitfield = Bitfield::new(num_blocks);
                block_bitfield.set_bytes(&block_buffer);
                pieces[p] = Some(Piece::from_slice(
                    Self::BLOCK_SIZE,
                    metadata.pieces[p],
                    &Self::read_piece_from_disk(&metadata, torrent_files, p).await?,
                    Some(&block_bitfield),
                ));
                active.insert(p);
            }
            for p in (0..metadata.num_pieces).filter(|p| !active.contains(p)) {
                let written = bitfield.is_set(p);
                pieces[p] = Some(Piece::new(
                    Some(p),
                    Self::BLOCK_SIZE,
                    metadata.piece_length(p),
                    metadata.pieces[p],
                    written
                ));
            }
        } else {
            for p in 0..metadata.num_pieces {
                pieces[p] = Some(Piece::new(
                    Some(p),
                    Self::BLOCK_SIZE,
                    metadata.piece_length(p),
                    metadata.pieces[p],
                    false
                ));
            }
        }
        Ok((pieces.into_iter().filter_map(|p| p).collect(), bitfield))
    }

    async fn read_piece_from_disk(
        metadata: &Metadata,
        torrent_files: &mut [File],
        index: usize,
    ) -> Result<Vec<u8>> {
        let mut piece = vec![0; metadata.piece_length(index)];

        for piece_file in &metadata.piece_files[index] {
            let src_file = &mut torrent_files[piece_file.file_index];

            src_file.seek(SeekFrom::Start(piece_file.file_offset as u64)).await?;

            src_file.read_exact(
                &mut piece[piece_file.piece_offset
                    ..piece_file.piece_offset + piece_file.length]
            ).await?;
        }

        Ok(piece)
    }

    async fn read_piece(&mut self, index: usize, begin: usize, length: usize) -> Result<Vec<u8>> {
        if let Some(piece) = self.piece_cache.get(index) {
            return Ok(piece[begin..begin + length].to_vec());
        }

        let piece = Self::read_piece_from_disk(
            &self.metadata,
            &mut self.torrent_files,
            index,
        ).await?;

        let read = piece[begin..begin + length].to_vec();
        self.piece_cache.insert(index, piece);

        Ok(read)
    }

    async fn write_block(&mut self, piece_index: usize, block_index: usize, block: &[u8]) -> Result<()> {
        let block_start = Self::BLOCK_SIZE * block_index;
        let block_end = (block_start + Self::BLOCK_SIZE).min(self.metadata.piece_length(piece_index));
        for piece_file in &self.metadata.piece_files[piece_index] {
            let overlap_start = block_start.max(piece_file.piece_offset);
            let overlap_end = block_end.min(piece_file.piece_offset + piece_file.length);
            if overlap_start < overlap_end {
                let dst_file = &mut self.torrent_files[piece_file.file_index];
                dst_file.seek(SeekFrom::Start(
                    (overlap_start - piece_file.piece_offset + piece_file.file_offset) as u64
                )).await?;
                dst_file.write_all(
                    &block[(overlap_start - block_start)..(overlap_end - block_start)]
                ).await?;
                debug!(piece = %piece_index, block = %block_index, "Written to {}",
                    self.metadata.files[piece_file.file_index].path.to_string_lossy());
            }
        }
        Ok(())
    }

    async fn request_block(
        &mut self,
        peer_id: &PeerId,
        piece_index: usize,
        block_index: usize,
    ) -> Result<()> {
        let span = info_span!("connection", id = %peer_id);
        let _enter = span.enter();
        let piece_length = self.metadata.piece_length(piece_index);
        let begin = Self::BLOCK_SIZE * block_index;
        let length = (piece_length - begin).min(Self::BLOCK_SIZE);
        if let Some(c) = self.connections.get_mut(peer_id) {
            c.request(piece_index, begin, length).await;
        }
        debug!("Requested block {piece_index}:{block_index}");
        Ok(())
    }

    fn find_or_keep_piece(&mut self, target_peer_id: &PeerId) -> Option<usize> {
        if self.piece_bitfield.is_all_set() {
            return None;
        }

        let target_connection = &self.connections[target_peer_id];
        // Connection already has a piece so keep it if still available
        if let Some(cursor) = target_connection.piece_cursor
            && self.pieces[cursor].is_available()
        {
            return Some(cursor);
        }
        // Check if other connections are already working on a piece that is available
        for con in self.connections.values() {
            if let Some(cursor) = con.piece_cursor
                && self.pieces[cursor].is_available()
                && target_connection.has_piece(cursor)
            {
                return Some(cursor);
            }
        }

        // Find new piece
        let sequential = self.progress_tx.borrow().sequential;
        for _ in 0..Self::PRIORITIES.len() {
            self.priority_cursor = (self.priority_cursor + 1) % Self::PRIORITIES.len();
            let pieces: Vec<_> = self.progress_tx.borrow()
                .transfer.as_ref().unwrap().files.iter().enumerate()
                .filter(|(_, f)| f.priority == Self::PRIORITIES[self.priority_cursor])
                .flat_map(|(f, _)| self.metadata.files[f].pieces.clone())
                .map(|p| (p, &self.pieces[p]))
                .collect();

            if sequential {
                for (p, piece) in pieces {
                    if piece.is_available() && target_connection.has_piece(p) {
                        return Some(p);
                    }
                }
            } else {
                let mut active = Vec::new();
                let mut inactive = Vec::new();
                for (p, piece) in pieces {
                    if piece.is_available() && target_connection.has_piece(p) {
                        if piece.is_active() {
                            active.push(p);
                        } else {
                            inactive.push(p);
                        }
                    }
                }
                if let Some(p) = active.choose(&mut rng())
                    .or_else(|| inactive.choose(&mut rng()))
                    .copied()
                {
                    return Some(p);
                }
            }
        }
        None
    }

    pub fn handle_command(&self, command: Command) {
        use Command::*;
        match command {
            SetPriority(index, priority) => {
                self.progress_tx.send_modify(|prog| {
                    prog.transfer.as_mut()
                        .and_then(|t| t.files.get_mut(index))
                        .map(|f| f.priority = priority);
                });
            }
            _ => (),
        }
    }

    async fn dispatch_requests(&mut self) -> Result<()> {
        let peer_ids: Vec<_> = self.connections.keys().copied().collect();
        for peer_id in peer_ids {
            while self.connections[&peer_id].can_request() {
                match self.find_or_keep_piece(&peer_id) {
                    Some(cursor) => {
                        self.connections.entry(peer_id)
                            .and_modify(|p| p.piece_cursor = Some(cursor));
                        if let Some(b) = self.pieces[cursor].find_available_block(&peer_id) {
                            self.request_block(&peer_id, cursor, b).await?;
                            self.pieces[cursor].download(b, peer_id);
                        } else {
                            break;
                        }
                    }
                    None => {
                        self.connections.entry(peer_id).and_modify(|c| c.piece_cursor = None);
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    fn downloaded_left(&self) -> (usize, usize) {
        let downloaded = (self.piece_bitfield.num_set() * self.metadata.piece_length)
            .min(self.metadata.length);
        let left = self.metadata.length - downloaded;
        (downloaded, left)
    }

    fn statistics(&mut self) {
        let mut downloaded_this_second = 0;
        let mut uploaded_this_second = 0;

        self.progress_tx.send_modify(|p| {
            for (id, con) in &self.connections {
                p.peers.entry(*id).and_modify(
                    |peer| peer.connection = Some(ConnectionProgress {
                        down_speed: con.downloaded_this_second(),
                        up_speed: con.uploaded_this_second(),
                        piece_bitfield: con.piece_bitfield().clone(),
                        am_choking: con.am_choking(),
                        am_interested: con.am_interested(),
                        peer_choking: con.peer_choking(),
                        peer_interested: con.peer_interested(),
                        pipeline: con.max_requests(),
                        piece_cursor: con.piece_cursor,
                        timeout_rate: con.timeouts_this_second(),
                        reject_rate: con.rejects_this_second(),
                    })
                );
            }
        });
        for con in self.connections.values_mut() {
            downloaded_this_second += con.downloaded_this_second();
            uploaded_this_second += con.uploaded_this_second();
            con.reset_stats();
        }

        self.uploaded += uploaded_this_second;
        self.progress_tx.send_modify(|p| {
            p.transfer.as_mut().map(|t| {
                t.down_speed = downloaded_this_second;
                t.up_speed = uploaded_this_second;
                t.uploaded = self.uploaded;
            });
        });
    }

    fn check_piece_index(&self, index: usize, op: &str) -> Result<()> {
        ensure!(
            index < self.metadata.num_pieces,
            "The index of {op} leads outside the number of pieces, got {index}",
        );
        Ok(())
    }

    fn check_piece_begin(&self, index: usize, begin: usize, boundary: bool, op: &str) -> Result<()> {
        self.check_piece_index(index, op)?;
        if boundary {
            ensure!(
                begin % Self::BLOCK_SIZE == 0,
                "Begin of {op} is not at block boundary, got {begin}",
            );
        }
        let piece_length = self.metadata.piece_length(index);
        ensure!(
            begin < piece_length,
            "Begin of {op} is past piece {index} of length {piece_length}, got {begin}",
        );
        Ok(())
    }

    fn check_piece_length(&self, index: usize, begin: usize, length: usize, boundary: bool, op: &str) -> Result<()> {
        self.check_piece_begin(index, begin, boundary, op)?;
        let piece_length = self.metadata.piece_length(index);
        let block_length = (piece_length - begin).min(Self::BLOCK_SIZE);
        if boundary {
            ensure!(
                length == block_length,
                "Length  of {op} does not match block, got {length}",
            );
        } else {
            ensure!(
                begin + length <= piece_length,
                "Length of {op} is past piece {index} of length {piece_length}, got {begin} + {length}",
            );
        }
        Ok(())
    }
}
