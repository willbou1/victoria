use anyhow::{Context, Result, anyhow, ensure};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        unix::SocketAddr,
    },
    select,
    sync::mpsc,
};
use tracing::trace;

use bencodedit::{BencodeValue, accessor::BencodeAccessor};

use super::{metadata::MetadataMessage, pex::PEXMessage};
use crate::libvictoria::{bitfield::Bitfield, torrent::Event, types::*};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum Message {
    KeepAlive,

    Choked(bool),
    Interested(bool),
    Bitfield(Bitfield),
    Have {
        piece_index: usize,
    },
    Request {
        index: usize,
        begin: usize,
        length: usize,
    },
    Cancel {
        index: usize,
        begin: usize,
        length: usize,
    },
    Piece {
        index: usize,
        begin: usize,
        data: Vec<u8>,
        response_time: Option<Duration>,
    },

    // BEP 6: https://www.bittorrent.org/beps/bep_0006.html
    Reject {
        index: usize,
        begin: usize,
        length: usize,
    },
    Suggest {
        index: usize,
    },
    AllowedFast {
        index: usize,
    },
    HaveAll,
    HaveNone,

    // BEP 10: https://bittorrent.org/beps/bep_0010.html
    ExtensionHandshake {
        extensions: HashMap<String, u8>,
        client: Option<String>,
        max_requests: Option<usize>,
        metadata_size: Option<usize>,
    },
    PEX(PEXMessage),
    Metadata(MetadataMessage),

    // BEP 5: https://bittorrent.org/beps/bep_0005.html
    DHTPort {
        port: u16,
    },

    UnsupportedExtension {
        type_byte: u8,
    },
    Unsupported {
        type_byte: u8,
    },
}

impl Message {
    pub fn into_bytes(self) -> Vec<u8> {
        match self {
            Self::ExtensionHandshake {
                extensions,
                client,
                max_requests,
                metadata_size,
            } => {
                let mut root = HashMap::new();
                if let Some(client) = client {
                    root.insert(vec![b'b'], BencodeValue::ByteString(client.into_bytes()));
                }
                if let Some(max_requests) = max_requests {
                    root.insert(
                        Vec::from(b"reqq"),
                        BencodeValue::Integer(max_requests as i64),
                    );
                }
                if let Some(metadata_size) = metadata_size {
                    root.insert(
                        Vec::from(b"metadata_size"),
                        BencodeValue::Integer(metadata_size as i64),
                    );
                }
                root.insert(
                    vec![b'm'],
                    BencodeValue::Dictionary(
                        extensions
                            .iter()
                            .map(|(name, id)| {
                                (name.as_bytes().to_vec(), BencodeValue::Integer(*id as i64))
                            })
                            .collect(),
                    ),
                );
                BencodeValue::Dictionary(root).to_bytes()
            }
            _ => Vec::new(),
        }
    }

    pub fn from_extension_handshake_bytes(encoded: &[u8]) -> Result<Self> {
        || -> Result<_> {
            let root_val = BencodeValue::from_bytes(encoded)?.0;
            let root = BencodeAccessor::new(&root_val);

            Ok(Self::ExtensionHandshake {
                extensions: root
                    .required(b"m")?
                    .map_dict(|name, id| {
                        Ok((String::from_utf8_lossy(name).to_string(), id.to_integer()?))
                    })?
                    .into_iter()
                    .collect(),
                client: root.map_optional(b"v", |v| v.to_string())?,
                max_requests: root.map_optional(b"reqq", |r| r.to_integer())?,
                metadata_size: root.map_optional(b"metadata_size", |s| s.to_integer())?,
            })
        }()
        .context("while parsing extension handshake")
    }
}

struct Handshake {
    advertise_extensions: bool,
    advertise_fast: bool,
    advertise_dht: bool,
    info_hash: Hash,
    peer_id: PeerId,
}

impl Handshake {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() == 68, "expected handshake to have 68 bytes");
        ensure!(bytes[0] == 19, "expected handshake to start with byte 19");
        ensure!(
            &bytes[1..20] == b"BitTorrent protocol",
            "expected handshake to have 'BitTorrent protocol' signature"
        );
        Ok(Self {
            advertise_fast: (bytes[20 + 7] & 0x04) > 0,
            advertise_extensions: (bytes[20 + 5] & 0x10) > 0,
            advertise_dht: (bytes[20 + 7] & 0x01) > 0,
            info_hash: bytes[28..48].try_into()?,
            peer_id: bytes[48..68].try_into()?,
        })
    }

    pub fn into_bytes(self) -> Vec<u8> {
        let mut handshake = vec![0u8; 68];
        handshake[0] = 19;
        handshake[1..20].copy_from_slice(b"BitTorrent protocol");
        handshake[20 + 5] |= 0x10;
        handshake[20 + 7] |= 0x04;
        handshake[20 + 7] |= 0x01;
        handshake[28..48].copy_from_slice(self.info_hash.as_ref());
        handshake[48..68].copy_from_slice(self.peer_id.as_ref());
        handshake
    }
}

type RequestKey = (usize, usize, usize);
type RequestTimes = Arc<tokio::sync::Mutex<HashMap<RequestKey, Instant>>>;
type ExtensionMapping = Arc<tokio::sync::Mutex<HashMap<String, u8>>>;

#[derive(Debug)]
pub struct BitTorrent {
    pub stream: TcpStream,
    pub name: String,
    pub id: PeerId,
    pub info_hash: Hash,
    pub supports_fast: bool,
    pub supports_dht: bool,
}

impl BitTorrent {
    const PEX_ID: u8 = 1;
    const METADATA_ID: u8 = 2;

    pub async fn receive_handshake(
        peer_info: &PeerInfo,
        known_info_hashes: &[Hash],
        local_id: &PeerId,
        mut stream: TcpStream,
    ) -> Result<Self> {
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            let mut handshake = [0u8; 68];
            let name = format!("{peer_info}");

            stream.read_exact(&mut handshake).await?;
            let query = Handshake::from_bytes(&handshake)?;
            trace!(peer = %name, "Received handshake");
            ensure!(query.peer_id != *local_id, "peer connected to itself");
            ensure!(
                known_info_hashes.contains(&query.info_hash),
                "peer connection for unknown info hash"
            );

            stream
                .write_all(
                    &Handshake {
                        advertise_dht: true,
                        advertise_extensions: true,
                        advertise_fast: true,
                        info_hash: query.info_hash,
                        peer_id: *local_id,
                    }
                    .into_bytes(),
                )
                .await?;
            trace!(peer = %name, "Sent handshake");

            Ok(Self {
                stream,
                name,
                id: query.peer_id,
                info_hash: query.info_hash,
                supports_fast: query.advertise_fast,
                supports_dht: query.advertise_dht,
            })
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(anyhow!("Exceeded handshake timeout")),
        }
    }

    pub async fn send_handshake(
        peer_info: &PeerInfo,
        info_hash: &Hash,
        local_id: &PeerId,
    ) -> Result<Self> {
        // TODO Make this try all endpoints
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            let mut handshake = [0u8; 68];
            let name = format!("{peer_info}");

            let addrs = peer_info.endpoints.resolve().await?;

            let mut stream = TcpStream::connect(addrs.iter().next().unwrap()).await?;
            trace!(peer = %name, "Connected");

            stream
                .write_all(
                    &Handshake {
                        advertise_dht: true,
                        advertise_extensions: true,
                        advertise_fast: true,
                        info_hash: *info_hash,
                        peer_id: *local_id,
                    }
                    .into_bytes(),
                )
                .await?;
            trace!(peer = %name, "Sent handshake");

            stream.read_exact(&mut handshake).await?;
            let response = Handshake::from_bytes(&handshake)?;
            ensure!(
                &response.info_hash == info_hash,
                "expected handshake response to have same info hash"
            );
            if let Some(peer_id) = peer_info.id {
                ensure!(
                    response.peer_id == peer_id,
                    "expected handshake response to have right peer id"
                );
            }
            trace!(peer = %name, "Received handshake");

            Ok(Self {
                stream,
                name,
                id: response.peer_id,
                info_hash: *info_hash,
                supports_fast: response.advertise_fast,
                supports_dht: response.advertise_dht,
            })
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(anyhow!("Exceeded handshake timeout")),
        }
    }

    pub async fn run(self, tx: mpsc::Sender<Event>, mut rx: mpsc::Receiver<Message>) {
        let BitTorrent {
            stream, name, id, ..
        } = self;

        let request_times = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let extensions = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let read_request_times = request_times.clone();
        let write_request_times = request_times.clone();
        let read_extensions = extensions.clone();
        let write_extensions = extensions.clone();

        let (mut reader, mut writer) = stream.into_split();

        let read_tx = tx.clone();
        let read_name = name.clone();

        let reader_task = async move {
            loop {
                match Self::receive(
                    &mut reader,
                    &read_name,
                    read_request_times.clone(),
                    read_extensions.clone(),
                )
                .await
                {
                    Ok(message) => {
                        if read_tx.send(Event::Message(id, message)).await.is_err() {
                            return Ok(());
                        }
                    }
                    Err(e) => {
                        return Err(e);
                    }
                }
            }
        };

        let writer_task = async move {
            // Send extension handshake TODO move this to torrent loop
            Self::send(
                &mut writer,
                &name,
                Message::ExtensionHandshake {
                    extensions: HashMap::from_iter(vec![
                        (String::from("ut_pex"), Self::PEX_ID),
                        (String::from("ut_metadata"), Self::METADATA_ID),
                    ]),
                    client: Some(format!("victoria/{}", env!("CARGO_PKG_VERSION"))),
                    // TODO respect this for other client
                    max_requests: Some(250),
                    metadata_size: None,
                },
                write_request_times.clone(),
                write_extensions.clone(),
            )
            .await?;

            let sleep = tokio::time::sleep(Duration::from_secs(60 * 2));
            tokio::pin!(sleep);
            loop {
                select! {
                    message = rx.recv() => {
                        match message {
                            Some(message) => {
                                Self::send(&mut writer, &name, message, write_request_times.clone(), write_extensions.clone()).await?;
                                sleep.as_mut().reset(
                                    tokio::time::Instant::now() + Duration::from_secs(60 * 2)
                                );
                            }
                            _ => break,
                        }
                    }
                    _ = &mut sleep => {
                        Self::send(&mut writer, &name, Message::KeepAlive, write_request_times.clone(), write_extensions.clone()).await?;
                        sleep.as_mut().reset(
                            tokio::time::Instant::now() + Duration::from_secs(60 * 2)
                        );
                    }
                }
            }

            Ok::<_, anyhow::Error>(())
        };

        select! {
            result = reader_task => {
                if let Err(e) = result {
                    let _ = tx.send(Event::Disconnection(id, e)).await;
                }
            }
            result = writer_task => {
                if let Err(e) = result {
                    let _ = tx.send(Event::Disconnection(id, e)).await;
                }
            }
        }
    }

    async fn send(
        writer: &mut OwnedWriteHalf,
        name: &str,
        message: Message,
        request_times: RequestTimes,
        extensions: ExtensionMapping,
    ) -> Result<()> {
        let mut payload = Vec::new();

        fn send_index_begin(payload: &mut Vec<u8>, index: usize, begin: usize) {
            payload.extend((index as u32).to_be_bytes());
            payload.extend((begin as u32).to_be_bytes());
        }

        trace!(peer = %name, "Sent {message:?}");
        match message {
            Message::Bitfield(bitfield) => {
                payload.push(5);
                payload.extend(bitfield.as_bytes());
            }
            Message::Have { piece_index: index } => {
                payload.push(4);
                payload.extend((index as u32).to_be_bytes());
            }
            Message::Request {
                index,
                begin,
                length,
            } => {
                payload.push(6);
                send_index_begin(&mut payload, index, begin);
                payload.extend((length as u32).to_be_bytes());
                request_times
                    .lock()
                    .await
                    .insert((index, begin, length), Instant::now());
            }
            Message::Cancel {
                index,
                begin,
                length,
            } => {
                payload.push(8);
                send_index_begin(&mut payload, index, begin);
                payload.extend((length as u32).to_be_bytes());
            }
            Message::Piece {
                index,
                begin,
                data: piece,
                ..
            } => {
                payload.push(7);
                send_index_begin(&mut payload, index, begin);
                payload.extend(piece);
            }
            Message::Choked(choked) => {
                payload.push(if choked { 0 } else { 1 });
            }
            Message::Interested(interested) => {
                payload.push(if interested { 2 } else { 3 });
            }

            // extensions
            Message::Reject {
                index,
                begin,
                length,
            } => {
                payload.push(16);
                send_index_begin(&mut payload, index, begin);
                payload.extend((length as u32).to_be_bytes());
            }
            Message::HaveAll => {
                payload.push(0x0E);
            }
            Message::HaveNone => {
                payload.push(0x0F);
            }
            Message::Suggest { index } => {
                payload.push(0x0D);
                payload.extend((index as u32).to_be_bytes());
            }
            Message::AllowedFast { index } => {
                payload.push(0x11);
                payload.extend((index as u32).to_be_bytes());
            }

            Message::DHTPort { port } => {
                payload.push(0x09);
                payload.extend(port.to_be_bytes());
            }

            Message::KeepAlive => {}
            Message::Unsupported { .. } => {}
            Message::UnsupportedExtension { .. } => {}
            msg @ Message::ExtensionHandshake { .. } => {
                payload.push(20);
                payload.push(0);
                payload.extend(msg.into_bytes());
            }
            Message::Metadata(msg) => {
                payload.push(20);
                payload.push(*extensions.lock().await.get("ut_metadata").unwrap());
                payload.extend(msg.into_bytes());
            }
            Message::PEX(msg) => {
                payload.push(20);
                payload.push(*extensions.lock().await.get("ut_pex").unwrap());
                payload.extend(msg.into_bytes());
            }
        }

        let length = u32::try_from(payload.len())?;
        writer.write_all(&length.to_be_bytes()).await?;
        writer.write_all(&payload).await?;
        Ok(())
    }

    fn check_length(expected: usize, actual: usize, at_least: bool, op: &str) -> Result<()> {
        ensure!(
            if at_least {
                actual >= expected
            } else {
                actual == expected
            },
            "The length of a {op} payloud should be{} {expected}, got {actual}",
            if at_least { " at least " } else { "" }
        );
        Ok(())
    }

    async fn receive(
        reader: &mut OwnedReadHalf,
        name: &str,
        request_times: RequestTimes,
        extensions: ExtensionMapping,
    ) -> Result<Message> {
        let mut int_buf = [0u8; 4];
        reader.read_exact(&mut int_buf).await?;
        let message_length = u32::from_be_bytes(int_buf) as usize;

        if message_length == 0 {
            return Ok(Message::KeepAlive);
        }

        async fn receive_index_begin(reader: &mut OwnedReadHalf) -> Result<(usize, usize)> {
            let mut int_buf = [0u8; 4];
            reader.read_exact(&mut int_buf).await?;
            let index = u32::from_be_bytes(int_buf) as usize;
            reader.read_exact(&mut int_buf).await?;
            let begin = u32::from_be_bytes(int_buf) as usize;
            Ok((index, begin))
        }

        let mut id_buf = [0u8; 1];
        reader.read_exact(&mut id_buf).await?;
        let msg = match id_buf[0] {
            5 => {
                // bitfield
                Self::check_length(2, message_length, true, "Bitfield")?;
                let mut bitfield = vec![0u8; message_length - 1];
                reader.read_exact(&mut bitfield).await?;
                Message::Bitfield(Bitfield::from_vec(bitfield))
            }
            4 => {
                // have
                Self::check_length(1 + 4, message_length, false, "Have")?;
                reader.read_exact(&mut int_buf).await?;
                let index = u32::from_be_bytes(int_buf) as usize;
                Message::Have { piece_index: index }
            }
            6 => {
                // request
                Self::check_length(1 + 4 * 3, message_length, false, "Request")?;
                let (index, begin) = receive_index_begin(reader).await?;
                reader.read_exact(&mut int_buf).await?;
                let length = u32::from_be_bytes(int_buf) as usize;
                Message::Request {
                    index,
                    begin,
                    length,
                }
            }
            7 => {
                // piece
                Self::check_length(1 + 4 * 2, message_length, true, "Piece")?;
                let (index, begin) = receive_index_begin(reader).await?;
                let mut piece = vec![0u8; message_length - 1 - 4 - 4];
                reader.read_exact(&mut piece).await?;

                let key = (index, begin, piece.len());
                let sent_at = request_times.lock().await.remove(&key);
                let response_time = sent_at.map(|t| t.elapsed());

                Message::Piece {
                    index,
                    begin,
                    data: piece,
                    response_time,
                }
            }
            8 => {
                // cancel
                Self::check_length(1 + 4 * 3, message_length, false, "Cancel")?;
                let (index, begin) = receive_index_begin(reader).await?;
                reader.read_exact(&mut int_buf).await?;
                let length = u32::from_be_bytes(int_buf) as usize;
                Message::Cancel {
                    index,
                    begin,
                    length,
                }
            }
            0 => {
                Self::check_length(1, message_length, false, "Choke")?;
                Message::Choked(true)
            }
            1 => {
                Self::check_length(1, message_length, false, "Unchoke")?;
                Message::Choked(false)
            }
            2 => {
                Self::check_length(1, message_length, false, "Interested")?;
                Message::Interested(true)
            }
            3 => {
                Self::check_length(1, message_length, false, "Uninterested")?;
                Message::Interested(false)
            }

            16 => {
                // reject
                Self::check_length(1 + 4 * 3, message_length, false, "Reject")?;
                let (index, begin) = receive_index_begin(reader).await?;
                reader.read_exact(&mut int_buf).await?;
                let length = u32::from_be_bytes(int_buf) as usize;
                Message::Reject {
                    index,
                    begin,
                    length,
                }
            }
            0x0E => {
                // HaveAll
                Self::check_length(1, message_length, false, "HaveAll")?;
                Message::HaveAll
            }
            0x0F => {
                // HaveNone
                Self::check_length(1, message_length, false, "HaveNone")?;
                Message::HaveNone
            }
            0x0D => {
                // suggest
                Self::check_length(1 + 4, message_length, false, "suggest")?;
                reader.read_exact(&mut int_buf).await?;
                let index = u32::from_be_bytes(int_buf) as usize;
                Message::Suggest { index }
            }
            0x11 => {
                // allow fast
                Self::check_length(1 + 4, message_length, false, "AllowFast")?;
                reader.read_exact(&mut int_buf).await?;
                let index = u32::from_be_bytes(int_buf) as usize;
                Message::AllowedFast { index }
            }

            0x09 => {
                // DHTPort
                Self::check_length(1 + 2, message_length, false, "DHTPort")?;
                reader.read_exact(&mut int_buf[0..2]).await?;
                let port = u16::from_be_bytes(int_buf[0..2].try_into().unwrap());
                Message::DHTPort { port }
            }

            // extensions
            20 => {
                let mut payload = vec![0u8; message_length - 1];
                reader.read_exact(&mut payload).await?;
                trace!(peer = %name,
                    "Received extension {{id: {}}}",
                    payload[0],
                );
                match payload[0] {
                    0 => {
                        let msg = Message::from_extension_handshake_bytes(&payload[1..])
                            .map_err(|e| anyhow!("{e}"))?;
                        if let Message::ExtensionHandshake {
                            extensions: new, ..
                        } = &msg
                        {
                            extensions.lock().await.extend(new.clone());
                        }
                        msg
                    }
                    Self::PEX_ID => Message::PEX(
                        PEXMessage::from_bytes(&payload[1..]).map_err(|e| anyhow!("{e}"))?,
                    ),
                    Self::METADATA_ID => Message::Metadata(
                        MetadataMessage::from_bytes(&payload[1..]).map_err(|e| anyhow!("{e}"))?,
                    ),
                    _ => Message::UnsupportedExtension {
                        type_byte: payload[0],
                    },
                }
            }

            _ => {
                let mut payload = vec![0u8; message_length - 1];
                reader.read_exact(&mut payload).await?;
                Message::Unsupported {
                    type_byte: id_buf[0],
                }
            }
        };
        trace!(peer = %name, "Received {msg:?}");
        Ok(msg)
    }
}
