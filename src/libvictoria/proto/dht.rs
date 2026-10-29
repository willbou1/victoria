use anyhow::{Context, Result, anyhow};
use tracing::{trace};

use tokio::{
    net::UdpSocket,
};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr, Ipv6Addr},
};
use rand::RngExt;

use bencodedit::{accessor::BencodeAccessor, BencodeValue};

use crate::libvictoria::types::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInfo {
    pub id: NodeId,
    pub endpoint: Endpoint,
}

impl NodeInfo {
    pub fn from_compact(compact: &[u8]) -> Result<Self> {
        if compact.len() != 26 {
            return Err(anyhow!("a compact node needs to be 26 bytes long"));
        }
        Ok(Self {
            id: NodeId(compact[0..20].try_into().unwrap()),
            endpoint: Endpoint::from_compact_4(&compact[20..26])?,
        })
    }
    
    pub fn from_compact_list(compact: &[u8]) -> Result<Vec<Self>> {
        if !compact.len().is_multiple_of(26) {
            return Err(anyhow!("a list of compact nodes need to be divisible by 26"));
        }
        Ok(compact.chunks_exact(26)
            .map(|c| Self::from_compact(c).unwrap())
            .collect())
    }

    pub fn to_compact(&self) -> Vec<u8> {
        let mut compact = Vec::new();
        compact.extend(self.id.as_ref());
        if let Endpoint::V4(v4) = &self.endpoint {
            compact.extend(v4.to_compact());
        } else {
            panic!("a compact node should only have an IPV4");
        }
        compact
    }
}

type TransactionId = Vec<u8>;
pub struct Message {
    transaction_id: TransactionId,
    client_version: Option<ClientVersion>,
    pub type_of: MessageType,
}

impl Message {
    pub fn from_bytes<F>(encoded: &[u8], mut f: F) -> Result<Self>
        where F: FnMut(&BencodeAccessor, char, &TransactionId) -> Result<MessageType>
    {
        let root_val = BencodeValue::from_bytes(encoded)?.0;
        let root = BencodeAccessor::new(&root_val);
        let transaction_id = root.required(b"t")?.to_bytes()?;
        let message_type = root.required(b"y")?.to_char()?;
        Ok(Self {
            client_version: root.map_optional(b"v", |v| v.to_bytes())?,
            type_of: f(&root, message_type, &transaction_id)?,
            transaction_id,
        })
    }

    pub fn to_bytes(self) -> Vec<u8> {
        use BencodeValue::*;
        let mut key_vals = HashMap::from([
            (b"t".into(), ByteString(self.transaction_id)),
        ]);
        if let Some(client_version) = self.client_version {
            key_vals.insert(b"v".into(),
                ByteString(client_version.to_vec()));
        }
        self.type_of.fill_bencode(&mut key_vals);
        Dictionary(key_vals).to_bytes()
    }
}

#[derive(Debug)]
pub enum ErrorCode {
    Generic = 201,
    Server = 202,
    Protocol = 203,
    UnknownMethod = 204,
}

impl TryFrom<u8> for ErrorCode {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            201 => Ok(Self::Generic),
            202 => Ok(Self::Server),
            203 => Ok(Self::Protocol),
            204 => Ok(Self::UnknownMethod),
            _ => Err(anyhow!("invalid error code {value}")),
        }
    }
}

pub enum MessageType {
    Query {
        id: NodeId,
        query: Query,
    },
    Response {
        id: NodeId,
        response: Response,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
}

impl MessageType {
    fn from_error_bencode(error: BencodeAccessor) -> Result<Self> {
        Ok(Self::Error {
            code: {
                let code = error.at(0)?;
                code.to_integer::<u8>()?.try_into()
                    .map_err(|e| code.wrap_err(e))?
            },
            message: error.at(1)?.to_string()?,
        })
    }

    pub fn from_bencode(root: &BencodeAccessor) -> Result<Self> {
        let y = root.required(b"y")?;
        Ok(match y.to_char()? {
            'q' => Self::Query {
                id: NodeId(root.required(b"a")?.required(b"id")?.to_n_bytes()?),
                query: Query::from_bencode(root)?,
            },
            'r' => {
                match (root.optional(b"r")?, root.optional(b"e")?) {
                    (Some(response), None) => Self::Response {
                        id: NodeId(response.required(b"id")?.to_n_bytes()?),
                        response: Response::from_bencode(&response)?,
                    },
                    (None, Some(error)) => Self::from_error_bencode(error)?,
                    _ => root.err_out(anyhow!("a response needs either a response or an error body"))?,
                }
            },
            'e' => Self::from_error_bencode(root.required(b"e")?)?,
            t => y.err_out(anyhow!("invalid message type {t} expected r, q or e"))?,
        })
    }

    pub fn fill_bencode(self, key_vals: &mut HashMap<Vec<u8>, BencodeValue>) {
        use BencodeValue::*;
        match self {
            Self::Error { code, message } => {
                key_vals.insert(b"y".into(), ByteString(b"e".to_vec()));
                key_vals.insert(b"e".into(), List(vec![
                    Integer(code as i64),
                    ByteString(message.bytes().collect())
                ]));
            }
            Self::Query { id, query } => {
                key_vals.insert(b"y".into(), ByteString(b"q".to_vec()));
                query.fill_bencode(key_vals, id);
            }
            Self::Response { id, response } => {
                key_vals.insert(b"y".into(), ByteString(b"r".to_vec()));
                key_vals.insert(b"r".into(), response.to_bencode(id));
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum Query {
    Ping,
    AnnouncePeer {
        implied_port: bool,
        info_hash: Hash,
        port: u16,
        token: Vec<u8>,
    },
    GetPeers {
        info_hash: Hash,
    },
    FindNode {
        target: NodeId,
    },
}

impl Query {
    pub fn from_bencode(root: &BencodeAccessor) -> Result<Self> {
        let q = root.required(b"q")?;
        let params = root.required(b"a")?;
        Ok(match q.as_str()? {
            "ping" => Self::Ping,
            "find_node" => Self::FindNode {
                target: NodeId(params.required(b"target")?.to_n_bytes()?),
            },
            "get_peers" => Self::GetPeers {
                info_hash: Hash(params.required(b"info_hash")?.to_n_bytes()?),
            },
            "announce_peer" => Self::AnnouncePeer {
                info_hash: Hash(params.required(b"info_hash")?.to_n_bytes()?),
                implied_port: params.required(b"implied_port")?.to_bool()?,
                port: params.required(b"port")?.to_integer()?,
                token: params.required(b"token")?.to_bytes()?,
            },
            t => q.err_out(anyhow!("invalid query type {t}"))?,
        })
    }

    pub fn fill_bencode(self, key_vals: &mut HashMap<Vec<u8>, BencodeValue>, id: NodeId) {
        use BencodeValue::*;
        let mut params = HashMap::from([
            (b"id".into(), ByteString(id.to_vec()))
        ]);
        match self {
            Self::Ping => {
                key_vals.insert(b"q".into(), ByteString(b"ping".to_vec()));
            },
            Self::FindNode { target } => {
                key_vals.insert(b"q".into(), ByteString(b"find_node".to_vec()));
                params.insert(b"target".into(), ByteString(target.to_vec()));
            }
            Self::GetPeers { info_hash } => {
                key_vals.insert(b"q".into(), ByteString(b"get_peers".to_vec()));
                params.insert(b"info_hash".into(), ByteString(info_hash.to_vec()));
            }
            Self::AnnouncePeer { implied_port, info_hash, port, token } => {
                key_vals.insert(b"q".into(), ByteString(b"announce_peer".to_vec()));
                params.insert(b"info_hash".into(), ByteString(info_hash.to_vec()));
                params.insert(b"token".into(), ByteString(token));
                params.insert(b"implied_port".into(), Integer(if implied_port {1} else {0}));
                params.insert(b"port".into(), Integer(port as i64));
            }
        }
        key_vals.insert(b"a".into(), Dictionary(params));
    }
}

#[derive(Debug)]
pub struct Response {
    pub token: Option<Vec<u8>>,
    pub nodes: Option<Vec<NodeInfo>>,
    pub values: Option<Vec<EndpointV4>>,
}

impl Response {
    pub fn from_bencode(response: &BencodeAccessor) -> Result<Self> {
        Ok(Self {
            token: response.map_optional(b"token", |token|
                token.to_bytes()
            )?,
            values: response.map_optional(b"values", |values|
                values.map_list(|value|
                    Ok(EndpointV4::from_compact(value.as_n_bytes()?))
                )
            )?,
            nodes: response.map_optional(b"nodes", |nodes|
                NodeInfo::from_compact_list(nodes.as_bytes()?)
                    .map_err(|e| nodes.wrap_err(e))
            )?,
        })
    }


    pub fn to_bencode(self, id: NodeId) -> BencodeValue {
        use BencodeValue::*;
        let mut response = HashMap::from([
            (b"id".into(), ByteString(id.to_vec()))
        ]);
        if let Some(nodes) = self.nodes {
            response.insert(b"nodes".into(), ByteString(
                nodes.iter().flat_map(|n| n.to_compact()).collect()
            ));
        }
        if let Some(values) = self.values {
            response.insert(b"values".to_vec(),  List(
                values.iter().map(|v| ByteString(v.to_compact().into())).collect()
            ));
        }
        if let Some(token) = self.token {
            response.insert(b"token".into(), ByteString(token));
        }
        Dictionary(response)
    }
}

pub struct DHT {
    id: NodeId,
    socket4: UdpSocket,
    buffer: [u8; 2048],
    queries: HashMap<TransactionId, Query>,
}

impl DHT {
    pub async fn bind(id: NodeId, port: u16) -> Result<Self> {
        Ok(Self {
            id,
            buffer: [0; _],
            socket4: UdpSocket::bind(
                SocketAddr::new(IpAddr::from(Ipv6Addr::UNSPECIFIED), port)
            ).await?,
            queries: HashMap::new(),
        })
    }

    pub async fn receive(&mut self) -> Result<(Endpoint, Message, Option<Query>)> {
        let (size, addr) = self.socket4.recv_from(&mut self.buffer).await?;
        let mut ret_query = None;
        let message = Message::from_bytes(&self.buffer[..size], |root, message_type, transaction_id| {
            match message_type {
                'q' => MessageType::from_bencode(root),
                'r' | 'e' => match self.queries.remove(transaction_id) {
                    Some(query) => {
                        ret_query = Some(query.clone());
                        MessageType::from_bencode(root)
                    }.context(format!("Associated with {query:?}")),
                    None => Err(anyhow!("Reply to unsolicited messag")),
                }
                c => Err(anyhow!("Unsupported message type  {c}"))
            }
        })?;
        Ok((Endpoint::from(addr), message, ret_query))
    }

    pub async fn query(
        &mut self,
        endpoint: &Endpoint,
        query: Query,
    ) -> Result<()> {
        let transaction_id: [u8; 2] = rand::rng().random();
        self.queries.insert(transaction_id.to_vec(), query.clone());
        self.socket4.send_to(
            &Message {
                transaction_id: transaction_id.to_vec(),
                client_version: Some(CLIENT_VERSION.to_vec()),
                type_of: MessageType::Query {
                    id: self.id,
                    query: query.clone(),
                },
            }.to_bytes(),
            endpoint.resolve().await?,
        ).await?;
        Ok(())
    }

    pub async fn respond(
        &self,
        endpoint: &Endpoint,
        transaction_id: Vec<u8>,
        response: Response,
    ) -> Result<()> {
        self.socket4.send_to(
            &Message {
                transaction_id,
                client_version: Some(CLIENT_VERSION.to_vec()),
                type_of: MessageType::Response {
                    id: self.id,
                    response,
                },
            }.to_bytes(),
            endpoint.resolve().await?,
        ).await?;
        Ok(())
    }
}
